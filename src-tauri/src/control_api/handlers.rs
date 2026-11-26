//! HTTP handlers and bearer-token middleware for the local control API.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::integrations::parse::{parse_input, ParseError};
use crate::integrations::token::tokens_equal;
use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use innertube::SongItem;
use listen_protocol::RemoteAction;

use super::types::{
    duration_secs, ApiJson, ApiQuery, ApiShared, ApiTrack, ErrorBody, NextResponse,
    NowPlayingResponse, OkResponse, PlaybackStateResponse, PreviousResponse, QueueQuery,
    QueueRequest, QueueRequestResponse, QueueResponse, RemoveRequest, ResolveError, SeekRequest,
    SeekResponse, SimpleQueueRequest, StatusResponse, VolumeRequest, VolumeResponse,
    CONTROL_API_RESOLVE_TIMEOUT, CONTROL_API_SKIP_HARD_TIMEOUT,
};

/// A mutation waits at most this long for the shared lock; longer waits answer `503 busy` with
/// `Retry-After: 10` instead of tying up an HTTP worker behind a stuck skip.
const CONTROL_API_MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP-side deadline for `POST /playback/next`. The underlying skip is detached, not aborted.
const CONTROL_API_NEXT_TIMEOUT: Duration = Duration::from_secs(15);

/// Accept only `Authorization: Bearer <token>`. No query-string fallback, no CORS headers. The
/// header and token are never logged, including on failure. Also used by the WebSocket handler to
/// reject before it calls `on_upgrade`.
pub(crate) async fn bearer_authorized(
    shared: &ApiShared,
    headers: &HeaderMap,
) -> Result<(), ErrorBody> {
    let supplied = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    match supplied {
        Some(value) if tokens_equal(&value, &shared.token.read().await) => Ok(()),
        _ => Err(ErrorBody::unauthorized()),
    }
}

async fn require_bearer(
    State(shared): State<Arc<ApiShared>>,
    req: Request,
    next: Next,
) -> Response {
    match bearer_authorized(&shared, req.headers()).await {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
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
        .route("/api/v1/ws", get(super::ws::ws_handler))
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

/// `GET /api/v1/now-playing`: the currently heard track and its transport facts. `now` is the
/// shared queue-row projection, so it carries `album_id`, `queued_by`, `submitted_by` and the
/// queue flags;
/// `position` and `duration` are seconds. See [`NowPlayingResponse`].
async fn now_playing(State(shared): State<Arc<ApiShared>>) -> Response {
    let snap = shared.context.playback_snapshot().await;
    let track = shared.context.current_api_track().await.unwrap_or_default();
    Json(NowPlayingResponse::from_snapshot(&snap, track)).into_response()
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
        return Err(ErrorBody::listen_together_guest_with(
            shared.context.shared_control_reason().await,
        ));
    }
    if shared.context.remote_linked() {
        return Err(ErrorBody::remote_linked());
    }
    Ok(())
}

/// A guest in an opted-in fork room forwards its mutation to the host and answers from the
/// correlated result. Everywhere else this returns `None` so the handler's normal guard and apply
/// path run unchanged. The response builder is only called after the host has answered.
async fn forward_if_guest<F, Fut>(
    shared: &ApiShared,
    action: RemoteAction,
    response: F,
) -> Result<Option<Response>, ErrorBody>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Response>,
{
    if !shared.context.is_guest().await {
        return Ok(None);
    }
    if !shared.context.shared_control_allowed().await {
        return Err(ErrorBody::listen_together_guest_with(
            shared.context.shared_control_reason().await,
        ));
    }
    shared
        .context
        .forward_shared_control(action)
        .await
        .map_err(ErrorBody::shared_control_failed)?;
    Ok(Some(response().await))
}

macro_rules! forward_guest {
    ($shared:expr, $action:expr, $response:expr) => {
        match forward_if_guest($shared, $action, $response).await {
            Ok(Some(response)) => return response,
            Ok(None) => {}
            Err(error) => return error.into_response(),
        }
    };
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
    forward_guest!(&shared, RemoteAction::Play, || async {
        Json(PlaybackStateResponse { ok: true, playing: true }).into_response()
    });
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().play().await;
    Json(PlaybackStateResponse { ok: true, playing }).into_response()
}

/// `POST /api/v1/playback/pause`: idempotent. Safe on an idle or already-paused queue.
async fn pause(State(shared): State<Arc<ApiShared>>) -> Response {
    forward_guest!(&shared, RemoteAction::Pause, || async {
        Json(PlaybackStateResponse { ok: true, playing: false }).into_response()
    });
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().pause().await;
    Json(PlaybackStateResponse { ok: true, playing }).into_response()
}

/// `POST /api/v1/playback/toggle`: flips transport state and reports the final state.
async fn toggle(State(shared): State<Arc<ApiShared>>) -> Response {
    // The mirrored snapshot is the guest's view of host state; Toggle is applied there, so the
    // post-command state is its inverse.
    let playing = !shared
        .context
        .playback_snapshot()
        .await
        .get("paused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    forward_guest!(&shared, RemoteAction::Toggle, || async {
        Json(PlaybackStateResponse { ok: true, playing }).into_response()
    });
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
/// cannot observe a half-applied skip (new track armed but autoplay fetch unfinished). The task has
/// a hard 45 s ceiling: a stalled resolver is cancelled, the app restores the pre-skip row, and the
/// mutation lock is released even though the HTTP response already timed out at 15 s. On expiry the
/// response is `next_timeout` with the track that was current when the request began. That response
/// does **not** by itself prove failure; clients should use an HTTP timeout of at least 20 s and
/// re-read `/status` before retrying.
async fn next(State(shared): State<Arc<ApiShared>>) -> Response {
    forward_guest!(&shared, RemoteAction::Next, || async {
        Json(NextResponse { ok: true, current: current_api_track(&shared).await }).into_response()
    });
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
        // The app's own skip restores its queue one second earlier. This outer guard also bounds
        // other implementations (including test doubles): after 45 s the future is dropped and the
        // owned guard is released no matter what it was awaiting.
        match tokio::time::timeout(
            CONTROL_API_SKIP_HARD_TIMEOUT,
            task_shared.context.clone().next(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => None,
        }
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
    forward_guest!(&shared, RemoteAction::Previous, || async {
        Json(PreviousResponse {
            ok: true,
            restarted: false,
            current: current_api_track(&shared).await,
        })
        .into_response()
    });
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
    if !shared.context.is_guest().await {
        if let Err(error) = guard(&shared).await {
            return error.into_response();
        }
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
    forward_guest!(
        &shared,
        RemoteAction::Seek { position_ms: (body.position_sec * 1000.0).round() as i64 },
        || async {
            Json(SeekResponse { ok: true, position_sec: body.position_sec }).into_response()
        }
    );
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
    if !shared.context.is_guest().await {
        if let Err(error) = guard(&shared).await {
            return error.into_response();
        }
    }
    if !(0..=100).contains(&body.volume) {
        return bad_request("invalid_volume", "volume must be an integer from 0 to 100")
            .into_response();
    }
    forward_guest!(
        &shared,
        RemoteAction::SetVolume { volume: body.volume as f64 / 100.0 },
        || async { Json(VolumeResponse { ok: true, volume: body.volume }).into_response() }
    );
    match shared.context.clone().set_volume(body.volume).await {
        Ok(()) => Json(VolumeResponse { ok: true, volume: body.volume }).into_response(),
        Err(message) => ErrorBody::playback_error(message).into_response(),
    }
}

fn row_video_id(row: &serde_json::Value) -> Option<&serde_json::Value> {
    row.get("video_id").or_else(|| row.get("videoId"))
}

/// A claimed row can be the requested song's counterpart, so look both fields up when finding the
/// row a forwarded request produced.
fn row_matches_id(row: &serde_json::Value, id: &str) -> bool {
    row_video_id(row).and_then(serde_json::Value::as_str) == Some(id)
        || row.get("counterpart").and_then(serde_json::Value::as_str) == Some(id)
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
    // A guest may be forwarding; `forward_guest!` below applies the same opt-in check before any
    // local mutation. Other callers still go through the shared guard first.
    if !shared.context.is_guest().await {
        if let Err(error) = guard(&shared).await {
            return error.into_response();
        }
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
    let id = snap
        .get("items")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.get(body.index))
        .and_then(row_video_id)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    forward_guest!(&shared, RemoteAction::Remove { index: body.index, id }, || async {
        Json(OkResponse { ok: true }).into_response()
    });
    shared.context.clone().remove_at(body.index).await;
    Json(OkResponse { ok: true }).into_response()
}

/// `POST /api/v1/queue/clear-queued`: clears manually queued rows via the existing app path.
async fn clear_queued(State(shared): State<Arc<ApiShared>>) -> Response {
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
    forward_guest!(&shared, RemoteAction::ClearQueued, || async {
        Json(OkResponse { ok: true }).into_response()
    });
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

/// The relay only accepts YouTube ids, which is also what a host can apply. A local-file row
/// cannot cross the wire; refusing it here keeps the guest's API from claiming a successful add.
fn shared_track(item: &SongItem) -> Option<listen_protocol::Track> {
    let track = listen_protocol::Track {
        id: item.video_id.clone(),
        title: item.title.clone(),
        artist: item.artists.clone(),
        thumbnail: item.thumbnail.clone(),
        duration_ms: duration_secs(item.duration.as_deref()).unwrap_or(0) as i64 * 1000,
        queued_by: item.queued_by.clone(),
        submitted_by: item.submitted_by.clone(),
    };
    crate::listentogether::shareable(&track).then_some(track)
}

/// After a forwarded add, answer with the guest's mirrored row. The host's broadcast reaches this
/// client through the same Listen Together state pump, so a missing row is represented by a default
/// track rather than a second round trip.
async fn forwarded_queue_response(shared: &ApiShared, video_id: &str) -> Response {
    let snap = shared.context.queue_snapshot().await;
    let items =
        snap.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    let position = items.iter().position(|row| row_matches_id(row, video_id)).unwrap_or(0);
    let row = items.get(position).cloned().unwrap_or(serde_json::Value::Null);
    Json(QueueRequestResponse { ok: true, position, item: ApiTrack::from_value(&row) })
        .into_response()
}

/// `POST /api/v1/queue/request`: resolve one song and add it to the queue. The response is
/// `{ok, position, item}`; `position` is the final zero-based index and `item` is the queued
/// [`ApiTrack`], including `is_video`, `duration_sec`, `queued_by` and `submitted_by`.
///
/// `max_duration_sec` measures the version that will actually play: the queued `video_id`'s own
/// duration, never its counterpart's. No extra `/player` lookup is made. It fails closed with 422
/// `duration_unknown` unless `allow_unknown_duration` is true; a known but longer track gets 422
/// `too_long` with `duration_sec` and `max_duration_sec`.
/// Rows inside the live `queue_dedupe_window` reject with 409 and the existing `position`; an
/// unowned row ahead of that window is moved and tagged instead. `allow_duplicates` skips this
/// policy, but the app's global `prevent_duplicates` setting still applies to a fresh insert. A
/// timeout from a previous skip never leaves this endpoint guessing: it holds the same mutation
/// lock as other current-track/queue changes.
async fn handle_queue_request(shared: Arc<ApiShared>, body: QueueRequest) -> Response {
    let parsed = match parse_input(&body.input) {
        Ok(parsed) => parsed,
        Err(error) => return parse_error(error).into_response(),
    };
    let mut item = match tokio::time::timeout(
        CONTROL_API_RESOLVE_TIMEOUT,
        shared.context.resolve_input(&parsed),
    )
    .await
    {
        Ok(Ok(item)) => item,
        Ok(Err(error)) => return resolve_error(error).into_response(),
        Err(_) => return ErrorBody::resolve_timeout().into_response(),
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
    if shared.context.is_guest().await {
        if !shared.context.shared_control_allowed().await {
            return ErrorBody::listen_together_guest_with(
                shared.context.shared_control_reason().await,
            )
            .into_response();
        }
        let Some(track) = shared_track(&item) else {
            return ErrorBody::shared_control_failed(
                "that track cannot be shared with the Listen Together host",
            )
            .into_response();
        };
        // `from` names an album/playlist source in the link protocol, not a requester. API
        // requests have no source, so leave it empty; attribution rides on the track itself.
        let from = None;
        let action = if body.play_next.unwrap_or(true) {
            RemoteAction::PlayNext { tracks: vec![track], from }
        } else {
            RemoteAction::AddToQueue { tracks: vec![track], from }
        };
        forward_guest!(&shared, action, || async {
            forwarded_queue_response(&shared, &item.video_id).await
        });
    }
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
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
    use crate::control_api::server::IntegrationsManager;
    use crate::db::Db;
    use crate::integrations::hub::HubEvent;
    use crate::integrations::parse::ParsedInput;
    use crate::integrations::settings::{ControlApiSettings, CONTROL_API_SETTINGS_KEY};
    use axum::body::Body;
    use axum::http::{header::AUTHORIZATION, HeaderValue, Request, StatusCode};
    use futures_util::StreamExt;
    use innertube::SongItem;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tokio::time::{sleep, timeout};
    use tokio_tungstenite_026::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite_026::tungstenite::Message as ClientMessage;
    use tokio_tungstenite_026::{connect_async, MaybeTlsStream, WebSocketStream};
    use tower::ServiceExt;

    use crate::state::QueueNeighbors;

    #[derive(Debug, Clone, Default)]
    struct MockRow {
        id: String,
        counterpart: Option<String>,
        autoplay: bool,
        queued: bool,
        queued_by: Option<String>,
        submitted_by: Option<String>,
        album: Option<String>,
        album_id: Option<String>,
    }

    #[derive(Debug)]
    struct MockContext {
        guest: bool,
        /// Opt-in state reported by the mock `shared_control_allowed`.
        shared_control: bool,
        /// Optional room blocker echoed in guest rejection messages.
        shared_control_reason: Option<String>,
        /// Actions the mock host received, in order.
        forwarded: StdMutex<Vec<RemoteAction>>,
        /// Simulated correlated failure from the host.
        forward_error: Option<String>,
        linked: bool,
        restarted: bool,
        hub: tokio::sync::broadcast::Sender<HubEvent>,
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
        resolve_delay: Duration,
        rows: StdMutex<Vec<MockRow>>,
        current: AtomicUsize,
        queue_dedupe_window: usize,
        inserted: StdMutex<Vec<(SongItem, bool, bool)>>,
    }

    impl Default for MockContext {
        fn default() -> Self {
            let (hub, _) = tokio::sync::broadcast::channel(64);
            Self {
                guest: false,
                shared_control: false,
                shared_control_reason: None,
                forwarded: StdMutex::new(Vec::new()),
                forward_error: None,
                linked: false,
                restarted: false,
                hub,
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
                resolve_delay: Duration::ZERO,
                rows: StdMutex::new(vec![
                    MockRow {
                        id: "abc".into(),
                        counterpart: None,
                        autoplay: false,
                        queued: false,
                        queued_by: Some("friend".into()),
                        submitted_by: Some("room_guest".into()),
                        album: Some("Mr. Brightside Album".into()),
                        album_id: Some("MPREb_LU3E5vWgwfG".into()),
                    },
                    MockRow {
                        id: "b".into(),
                        counterpart: None,
                        autoplay: false,
                        queued: false,
                        queued_by: Some("friend".into()),
                        submitted_by: Some("room_guest".into()),
                        ..Default::default()
                    },
                    MockRow {
                        id: "c".into(),
                        counterpart: None,
                        autoplay: true,
                        queued: false,
                        queued_by: None,
                        ..Default::default()
                    },
                ]),
                current: AtomicUsize::new(0),
                queue_dedupe_window: 10,
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
                "now": { "videoId": "abc", "queued_by": "friend", "submitted_by": "room_guest" },
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
                        "album": row.album,
                        "album_id": row.album_id,
                    });
                    if let Some(by) = &row.queued_by {
                        item.as_object_mut().unwrap().insert("queued_by".into(), json!(by));
                    }
                    if let Some(by) = &row.submitted_by {
                        item.as_object_mut().unwrap().insert("submitted_by".into(), json!(by));
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

        fn ui_events(&self) -> tokio::sync::broadcast::Sender<HubEvent> {
            self.hub.clone()
        }

        async fn is_guest(&self) -> bool {
            self.guest
        }

        async fn shared_control_allowed(&self) -> bool {
            self.guest && self.shared_control
        }

        async fn shared_control_reason(&self) -> Option<String> {
            self.shared_control_reason.clone()
        }

        async fn forward_shared_control(&self, action: RemoteAction) -> Result<(), String> {
            self.forwarded.lock().unwrap().push(action);
            match self.forward_error.clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
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
            if !self.resolve_delay.is_zero()
                && !matches!(parsed, ParsedInput::Query(query) if query.contains("next"))
            {
                sleep(self.resolve_delay).await;
            }
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
                let queue: Vec<SongItem> = rows
                    .iter()
                    .map(|row| SongItem {
                        video_id: row.id.clone(),
                        counterpart: row.counterpart.clone(),
                        queued_by: row.queued_by.clone(),
                        queued: row.queued,
                        ..Default::default()
                    })
                    .collect();
                let decision = if allow_duplicates {
                    super::super::types::RequestDecision::Insert
                } else {
                    super::super::types::classify_request(
                        &item,
                        queue.iter(),
                        self.current.load(Ordering::SeqCst),
                        self.queue_dedupe_window,
                    )
                };
                let row = match decision {
                    super::super::types::RequestDecision::Reject(matched) => Err(matched),
                    super::super::types::RequestDecision::Claim { index } => {
                        let mut claimed = rows.remove(index);
                        claimed.autoplay = false;
                        claimed.queued = next;
                        claimed.queued_by = item.queued_by.clone();
                        if item.submitted_by.is_some() {
                            claimed.submitted_by = item.submitted_by.clone();
                        }
                        Ok(claimed)
                    }
                    super::super::types::RequestDecision::Insert => Ok(MockRow {
                        id: item.video_id.clone(),
                        counterpart: item.counterpart.clone(),
                        autoplay: false,
                        queued: next,
                        queued_by: item.queued_by.clone(),
                        submitted_by: item.submitted_by.clone(),
                        album: item.album.clone(),
                        album_id: item.album_id.clone(),
                    }),
                }
                .and_then(|mut row| {
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
                    let mut inserted = SongItem {
                        video_id: row.id.clone(),
                        counterpart: row.counterpart.clone(),
                        album: row.album.clone(),
                        album_id: row.album_id.clone(),
                        queued_by: row.queued_by.clone(),
                        submitted_by: row.submitted_by.clone(),
                        ..item.clone()
                    };
                    inserted.queued = next;
                    inserted.queued_end = at + 1 == rows.len();
                    row.queued = next;
                    rows.insert(at, row);
                    Ok((at, inserted))
                });
                row
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
            hub: context.hub.clone(),
            ws: Arc::new(super::super::ws::WsConnections::default()),
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

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn spawn_ws_server(
    ) -> (String, Arc<ApiShared>, Arc<MockContext>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (app, context, shared) = test_router_shared(MockContext::default());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("ws://{addr}/api/v1/ws"), shared, context, task)
    }

    async fn connect_ws(
        url: &str,
        auth: bool,
    ) -> Result<ClientSocket, tokio_tungstenite_026::tungstenite::Error> {
        let mut request = url.into_client_request().unwrap();
        if auth {
            request.headers_mut().insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123"));
        }
        connect_async(request).await.map(|(socket, _)| socket)
    }

    async fn next_json(socket: &mut ClientSocket) -> Value {
        loop {
            match socket.next().await.unwrap().unwrap() {
                ClientMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
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
        assert_eq!(json["now"]["submitted_by"], "room_guest");
        assert_eq!(json["position"], 1.0);
    }

    #[tokio::test]
    async fn now_playing_carries_queued_by_and_seconds() {
        let response = get("/api/v1/now-playing", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["now"]["queued_by"], "friend");
        assert_eq!(json["now"]["submitted_by"], "room_guest");
        assert_eq!(json["now"]["album"], "Mr. Brightside Album");
        assert_eq!(json["now"]["album_id"], "MPREb_LU3E5vWgwfG");
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
        assert_eq!(json["next"]["submitted_by"], "room_guest");
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
    async fn guest_rejection_names_the_missing_capability() {
        let reason = "this Listen Together server does not support shared control; use this fork's sync-server on both devices";
        let (app, _) = test_router_with(MockContext {
            guest: true,
            shared_control_reason: Some(reason.to_string()),
            ..Default::default()
        });
        for (path, body) in [
            ("/api/v1/playback/play", json!({})),
            ("/api/v1/queue/request", json!({ "input": "a song" })),
        ] {
            let response = request(app.clone(), path, body, Some("Bearer abc123")).await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["error"], "listen_together_guest", "{path}");
            assert!(
                json["message"].as_str().is_some_and(|message| message.contains(reason)),
                "{path}: {json}"
            );
        }
    }

    #[tokio::test]
    async fn opted_in_guest_forwards_play_without_mutating_the_local_player() {
        let (app, ctx) = test_router_with(MockContext {
            guest: true,
            shared_control: true,
            ..Default::default()
        });
        let response =
            request(app, "/api/v1/playback/play", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["playing"], true);
        assert!(matches!(&ctx.forwarded.lock().unwrap()[..], [RemoteAction::Play]));
        assert_eq!(ctx.play_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn forwarded_control_failure_is_a_gateway_error() {
        let (app, ctx) = test_router_with(MockContext {
            guest: true,
            shared_control: true,
            forward_error: Some("the host did not answer within 15 seconds".into()),
            ..Default::default()
        });
        let response =
            request(app, "/api/v1/playback/pause", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let json = json_body(response).await;
        assert_eq!(json["error"], "shared_control_failed");
        assert!(json["message"].as_str().unwrap().contains("15 seconds"));
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 0);
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
    async fn stalled_skip_has_a_hard_ceiling_and_releases_the_lock() {
        let (app, ctx) = test_router_with(MockContext {
            delay: Duration::from_secs(120),
            next_item: StdMutex::new(Some(json!({ "videoId": "never" }))),
            ..Default::default()
        });
        let response =
            request(app.clone(), "/api/v1/playback/next", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json_body(response).await["error"], "next_timeout");

        // The 15 s response already came back; the detached task still owns the guard. At 45 s the
        // hard ceiling cancels it, and the next mutation must not see `busy`.
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        let response =
            request(app, "/api/v1/queue/clear-queued", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["ok"], true);
        assert_eq!(ctx.clears.load(Ordering::SeqCst), 1);
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

    #[tokio::test(start_paused = true)]
    async fn queue_request_resolve_timeout_returns_504_and_queues_nothing() {
        let (app, ctx) = test_router_with(MockContext {
            resolve_delay: Duration::from_secs(60),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "stalled" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let json = json_body(response).await;
        assert_eq!(json["error"], "resolve_timeout");
        assert!(ctx.inserted.lock().unwrap().is_empty());
        assert_eq!(ctx.rows.lock().unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_resolve_does_not_block_pause_and_volume() {
        let (app, ctx) = test_router_with(MockContext {
            resolve_delay: Duration::from_secs(60),
            ..Default::default()
        });

        let stalled = tokio::spawn(request(
            app.clone(),
            "/api/v1/queue/request",
            json!({ "input": "stalled" }),
            Some("Bearer abc123"),
        ));
        tokio::task::yield_now().await;

        // Pause and volume take no network; they must not queue behind the stalled resolve.
        let pause = tokio::spawn(request(
            app.clone(),
            "/api/v1/playback/pause",
            json!({}),
            Some("Bearer abc123"),
        ));
        let volume = tokio::spawn(request(
            app.clone(),
            "/api/v1/playback/volume",
            json!({ "volume": 50 }),
            Some("Bearer abc123"),
        ));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(pause.is_finished());
        assert!(volume.is_finished());
        assert!(!stalled.is_finished());
        tokio::time::advance(Duration::from_secs(13)).await;
        let paused = pause.await.unwrap();
        let volume_response = volume.await.unwrap();
        let stalled_response = stalled.await.unwrap();

        assert_eq!(paused.status(), StatusCode::OK);
        assert_eq!(json_body(paused).await["ok"], true);
        assert_eq!(volume_response.status(), StatusCode::OK);
        assert_eq!(json_body(volume_response).await["ok"], true);
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.volume_calls.load(Ordering::SeqCst), 1);
        assert_eq!(stalled_response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json_body(stalled_response).await["error"], "resolve_timeout");
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
        for (id, expected) in [("abc", 0), ("b", 1)] {
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
    async fn queue_request_window_rejects_claims_and_adds_by_distance() {
        // The mock resolver always returns `resolved123`; only its position and ownership vary.
        let near = test_router_with(MockContext {
            rows: StdMutex::new(vec![
                MockRow { id: "current".into(), ..Default::default() },
                MockRow { id: "resolved123".into(), ..Default::default() },
                MockRow { id: "spacer".into(), ..Default::default() },
            ]),
            queue_dedupe_window: 1,
            ..Default::default()
        })
        .0;
        let response = request(
            near,
            "/api/v1/queue/request",
            json!({ "input": "song", "requested_by": "bob" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(response).await["position"], 1);

        let claim = test_router_with(MockContext {
            rows: StdMutex::new(vec![
                MockRow { id: "current".into(), ..Default::default() },
                MockRow { id: "near".into(), ..Default::default() },
                MockRow { id: "spacer".into(), ..Default::default() },
                MockRow { id: "resolved123".into(), ..Default::default() },
                MockRow {
                    id: "far-owned".into(),
                    queued_by: Some("alice".into()),
                    ..Default::default()
                },
            ]),
            queue_dedupe_window: 1,
            ..Default::default()
        })
        .0;
        let response = request(
            claim,
            "/api/v1/queue/request",
            json!({ "input": "song", "requested_by": "bob", "play_next": false }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = json_body(response).await;
        assert_eq!(response["position"], 4);
        assert_eq!(response["item"]["video_id"], "resolved123");
        assert_eq!(response["item"]["queued_by"], "bob");

        let owned = test_router_with(MockContext {
            rows: StdMutex::new(vec![
                MockRow { id: "current".into(), ..Default::default() },
                MockRow { id: "near".into(), ..Default::default() },
                MockRow { id: "spacer".into(), ..Default::default() },
                MockRow {
                    id: "resolved123".into(),
                    queued_by: Some("alice".into()),
                    ..Default::default()
                },
            ]),
            queue_dedupe_window: 1,
            ..Default::default()
        })
        .0;
        let response = request(
            owned,
            "/api/v1/queue/request",
            json!({ "input": "song", "requested_by": "bob" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["item"]["video_id"], "resolved123");
    }

    #[tokio::test]
    async fn claimed_row_is_protected_from_the_next_request() {
        let (app, _) = test_router_with(MockContext {
            resolve_item: resolved("far", Some("3:00")),
            rows: StdMutex::new(vec![
                MockRow { id: "current".into(), ..Default::default() },
                MockRow { id: "near".into(), ..Default::default() },
                MockRow { id: "spacer".into(), ..Default::default() },
                MockRow { id: "far".into(), ..Default::default() },
            ]),
            queue_dedupe_window: 1,
            ..Default::default()
        });

        let first = request(
            app.clone(),
            "/api/v1/queue/request",
            json!({ "input": "far", "requested_by": "bob" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let first = json_body(first).await;
        assert_eq!(first["position"], 1);
        assert_eq!(first["item"]["video_id"], "far");
        assert_eq!(first["item"]["queued_by"], "bob");

        let second = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "far", "requested_by": "carol" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(second).await["position"], 1);
    }

    #[tokio::test]
    async fn counterpart_claim_returns_the_queued_row() {
        let (app, _) = test_router_with(MockContext {
            resolve_item: resolved("audio", Some("3:00")),
            rows: StdMutex::new(vec![
                MockRow { id: "current".into(), ..Default::default() },
                MockRow {
                    id: "music-video".into(),
                    counterpart: Some("audio".into()),
                    ..Default::default()
                },
            ]),
            queue_dedupe_window: 0,
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "audio", "requested_by": "bob" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["position"], 1);
        assert_eq!(json["item"]["video_id"], "music-video");
        assert_eq!(json["item"]["queued_by"], "bob");
    }

    #[test]
    fn forwarded_queue_response_matches_counterparts() {
        let row = json!({ "video_id": "music-video", "counterpart": "audio" });
        assert!(row_matches_id(&row, "audio"));
        assert!(row_matches_id(&row, "music-video"));
        assert!(!row_matches_id(&row, "other"));
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
            ..Default::default()
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
                ..Default::default()
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

    #[tokio::test]
    async fn ws_rejects_a_missing_bearer_header_before_upgrade() {
        let (url, _, _, task) = spawn_ws_server().await;
        match connect_ws(&url, false).await {
            Err(tokio_tungstenite_026::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let body = response.into_body().unwrap_or_default();
                let json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["error"], "unauthorized");
            }
            other => panic!("expected a 401 handshake, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn ws_ignores_a_query_string_token() {
        let (url, _, _, task) = spawn_ws_server().await;
        match connect_ws(&format!("{url}?token=abc123"), false).await {
            Err(tokio_tungstenite_026::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected a query token to be ignored, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn ws_hello_carries_the_slim_snapshot() {
        let (url, shared, _, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        assert_eq!(shared.ws.count(), 1);
        let hello = next_json(&mut socket).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["current"]["video_id"], "abc");
        assert_eq!(hello["current"]["album"], "Mr. Brightside Album");
        assert_eq!(hello["current"]["album_id"], "MPREb_LU3E5vWgwfG");
        assert_eq!(hello["position_sec"], 1.0);
        assert_eq!(hello["duration_sec"], 10.0);
        assert_eq!(hello["playing"], true);
        assert_eq!(hello["volume"], 100);
        assert_eq!(hello["current_index"], 0);
        assert_eq!(hello["total"], 3);
        assert_eq!(hello["listen_together_role"], "none");
        assert!(hello.get("next_is_autoplay_fetch").is_some());
        socket.close(None).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn ws_delivers_a_hub_event() {
        let (url, _, context, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        next_json(&mut socket).await;
        context.hub.send(HubEvent::Volume(42)).unwrap();
        let event = next_json(&mut socket).await;
        assert_eq!(event["type"], "volume");
        assert_eq!(event["volume"], 42);
        socket.close(None).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn ws_now_playing_uses_the_queue_row_for_album_fields() {
        let (url, _, context, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        next_json(&mut socket).await;
        // The slim UI event has no album at all; the queue row does.
        context
            .hub
            .send(HubEvent::NowPlaying(serde_json::json!({ "videoId": "abc", "album": null })))
            .unwrap();
        let event = next_json(&mut socket).await;
        assert_eq!(event["type"], "now_playing");
        assert_eq!(event["current"]["video_id"], "abc");
        assert_eq!(event["current"]["album"], "Mr. Brightside Album");
        assert_eq!(event["current"]["album_id"], "MPREb_LU3E5vWgwfG");
        socket.close(None).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn ws_sends_an_explicit_position_event() {
        let (url, _, context, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        next_json(&mut socket).await;
        context.hub.send(HubEvent::PositionSet(42.0)).unwrap();
        let event = next_json(&mut socket).await;
        assert_eq!(event["type"], "position");
        assert_eq!(event["position_sec"], 42.0);
        socket.close(None).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn ws_lagged_subscriber_gets_a_resync() {
        let (url, _, context, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        next_json(&mut socket).await;
        for volume in 0..100 {
            context.hub.send(HubEvent::Volume(volume)).unwrap();
        }
        let resync = next_json(&mut socket).await;
        assert_eq!(resync["type"], "resync");
        assert_eq!(resync["current"]["video_id"], "abc");
        assert_eq!(resync["total"], 3);
        socket.close(None).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn ws_caps_connections_at_eight() {
        let (url, shared, _, task) = spawn_ws_server().await;
        let mut sockets = Vec::new();
        for _ in 0..8 {
            let mut socket = connect_ws(&url, true).await.unwrap();
            assert_eq!(next_json(&mut socket).await["type"], "hello");
            sockets.push(socket);
        }
        assert_eq!(shared.ws.count(), 8);
        match connect_ws(&url, true).await {
            Err(tokio_tungstenite_026::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                let body = response.into_body().unwrap_or_default();
                let json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["error"], "too_many_clients");
            }
            other => panic!("expected a 503 handshake, got {other:?}"),
        }
        for mut socket in sockets {
            let _ = socket.close(None).await;
        }
        task.abort();
    }

    #[tokio::test]
    async fn ws_closes_on_token_regeneration_signal() {
        let (url, shared, _, task) = spawn_ws_server().await;
        let mut socket = connect_ws(&url, true).await.unwrap();
        next_json(&mut socket).await;
        shared.ws.close_all("control API token regenerated");
        match socket.next().await.unwrap().unwrap() {
            ClientMessage::Close(Some(frame)) => {
                assert!(frame.reason.to_string().contains("token regenerated"));
            }
            other => panic!("expected a close frame with a reason, got {other:?}"),
        }
        task.abort();
    }

    fn temp_db(label: &str) -> (Arc<Db>, std::path::PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("limusic-control-api-{label}-{}.db", std::process::id()));
        cleanup_db(&path);
        let db = Arc::new(Db::open(&path).unwrap());
        (db, path)
    }

    fn cleanup_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    async fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    fn control_settings(port: u16, enabled: bool) -> ControlApiSettings {
        ControlApiSettings {
            enabled,
            bind: "127.0.0.1".into(),
            port,
            shared_control: false,
            queue_dedupe_window: 10,
        }
    }

    async fn start_manager(db: Arc<Db>, port: u16, token: &str) -> IntegrationsManager {
        let manager = IntegrationsManager::new_for_test(
            db.clone(),
            Arc::new(MockContext::default()),
            token.to_string(),
        );
        let config = control_settings(port, true);
        db.set_setting(CONTROL_API_SETTINGS_KEY, &serde_json::to_string(&config).unwrap());
        manager.apply_settings(config, "settings changed").await;
        assert!(manager.control_api_status().await.running);
        manager
    }

    async fn connect_ws_as(url: &str, token: &str) -> ClientSocket {
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {token}")).unwrap());
        connect_async(request).await.unwrap().0
    }

    async fn assert_close(socket: &mut ClientSocket, expected: &str) {
        let message = timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("socket did not close in time")
            .expect("socket ended without a close frame")
            .expect("socket error while waiting for close");
        match message {
            ClientMessage::Close(Some(frame)) => {
                let reason = frame.reason.to_string();
                assert!(reason.contains(expected), "close reason {reason:?} lacks {expected:?}");
            }
            other => panic!("expected a close frame, got {other:?}"),
        }
    }

    async fn http_status(port: u16, token: &str) -> Option<StatusCode> {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/v1/status"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .ok()
            .map(|response| response.status())
    }

    #[tokio::test]
    async fn manager_token_regeneration_closes_sockets_and_rotates_token() {
        let (db, path) = temp_db("regen");
        let port = free_port().await;
        let old_token = "a".repeat(64);
        let manager = start_manager(db, port, &old_token).await;
        let mut socket =
            connect_ws_as(&format!("ws://127.0.0.1:{port}/api/v1/ws"), &old_token).await;
        next_json(&mut socket).await;
        assert_eq!(http_status(port, &old_token).await, Some(StatusCode::OK));

        let new_token = manager.regenerate_token().await.unwrap();

        assert_ne!(new_token, old_token);
        assert_close(&mut socket, "token regenerated").await;
        assert_eq!(http_status(port, &old_token).await, Some(StatusCode::UNAUTHORIZED));
        assert_eq!(http_status(port, &new_token).await, Some(StatusCode::OK));
        cleanup_db(&path);
    }

    #[tokio::test]
    async fn manager_disable_closes_sockets_and_stops_listener() {
        let (db, path) = temp_db("disable");
        let port = free_port().await;
        let token = "b".repeat(64);
        let manager = start_manager(db.clone(), port, &token).await;
        let mut socket = connect_ws_as(&format!("ws://127.0.0.1:{port}/api/v1/ws"), &token).await;
        next_json(&mut socket).await;

        let config = control_settings(port, false);
        db.set_setting(CONTROL_API_SETTINGS_KEY, &serde_json::to_string(&config).unwrap());
        manager.apply_settings(config, "settings changed").await;

        assert_close(&mut socket, "disabled").await;
        assert_eq!(http_status(port, &token).await, None);
        assert!(!manager.control_api_status().await.running);
        cleanup_db(&path);
    }

    #[tokio::test]
    async fn manager_port_change_closes_sockets_and_moves_listener() {
        let (db, path) = temp_db("port-change");
        let old_port = free_port().await;
        let new_port = loop {
            let port = free_port().await;
            if port != old_port {
                break port;
            }
        };
        let token = "c".repeat(64);
        let manager = start_manager(db.clone(), old_port, &token).await;
        let mut socket =
            connect_ws_as(&format!("ws://127.0.0.1:{old_port}/api/v1/ws"), &token).await;
        next_json(&mut socket).await;

        let config = control_settings(new_port, true);
        db.set_setting(CONTROL_API_SETTINGS_KEY, &serde_json::to_string(&config).unwrap());
        manager.apply_settings(config, "port change").await;

        assert_close(&mut socket, "port changed").await;
        assert_eq!(http_status(old_port, &token).await, None);
        assert_eq!(http_status(new_port, &token).await, Some(StatusCode::OK));
        cleanup_db(&path);
    }

    #[tokio::test]
    async fn manager_stop_closes_sockets_on_app_exit() {
        let (db, path) = temp_db("exit");
        let port = free_port().await;
        let token = "d".repeat(64);
        let manager = start_manager(db, port, &token).await;
        let mut socket = connect_ws_as(&format!("ws://127.0.0.1:{port}/api/v1/ws"), &token).await;
        next_json(&mut socket).await;

        manager.stop().await;

        assert_close(&mut socket, "shutting down").await;
        assert_eq!(http_status(port, &token).await, None);
        cleanup_db(&path);
    }
}
