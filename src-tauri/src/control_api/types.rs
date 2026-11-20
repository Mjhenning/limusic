//! Shared request state and response shapes for the local control API.

use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use innertube::SongItem;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::integrations::parse::ParsedInput;
use crate::state::QueueNeighbors;

/// The standard error envelope. `error` is a stable machine code; `message` is human-readable.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: &'static str,
    pub message: String,
    /// Present on `next_timeout`: the track that was current when the wait expired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<Value>,
    /// Existing queue index on a `duplicate` rejection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_sec: Option<u64>,
    #[serde(skip)]
    status: StatusCode,
}

impl ErrorBody {
    pub fn new(status: StatusCode, error: &'static str, message: impl Into<String>) -> Self {
        Self {
            error,
            message: message.into(),
            current: None,
            position: None,
            duration_sec: None,
            max_duration_sec: None,
            status,
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid Authorization: Bearer <token> header is required",
        )
    }

    pub fn listen_together_guest() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "listen_together_guest",
            "the Listen Together host controls playback",
        )
    }

    pub fn remote_linked() -> Self {
        Self::new(StatusCode::CONFLICT, "remote_linked", "another linked device is the player")
    }

    pub fn next_timeout(current: Option<Value>) -> Self {
        let mut error = Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "next_timeout",
            "the next track did not become current; the skip may still have completed",
        );
        error.current = current;
        error
    }

    pub fn queue_changed() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "queue_changed",
            "the queue row changed before the request was applied",
        )
    }

    pub fn duplicate(position: usize) -> Self {
        let mut error =
            Self::new(StatusCode::CONFLICT, "duplicate", "that video is already in the queue");
        error.position = Some(position);
        error
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no matching song was found")
    }

    pub fn resolve_failed(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "resolve_failed", message)
    }

    pub fn duration_unknown() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "duration_unknown",
            "the track has no known duration; set allow_unknown_duration to accept it",
        )
    }

    pub fn too_long(duration_sec: u64, max_duration_sec: u64) -> Self {
        let mut error = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_long",
            "the track is longer than max_duration_sec",
        );
        error.duration_sec = Some(duration_sec);
        error.max_duration_sec = Some(max_duration_sec);
        error
    }

    pub fn playback_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "playback_error", message)
    }
}

impl IntoResponse for ErrorBody {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// `GET /api/v1/status`. The full playback snapshot plus device context; never the bearer token.
///
/// `now` mirrors the UI's `now-playing` object and carries `queued_by` when the current row was
/// attributed. `paused` is the transport's paused flag. `position` and `duration` are **seconds**.
/// `volume` is an integer 0–100. `speed` is mpv's playback rate, `semitones` its pitch shift, and
/// `reverb` the full effect settings object. `live` is present only for a remote broadcast.
/// `linked` and `remote_linked` report another device driving this one; `listen_together_role` is
/// `"none"`, `"host"`, or `"guest"`.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusResponse {
    pub now: Value,
    pub paused: bool,
    /// Playback position in seconds.
    pub position: f64,
    /// Track length in seconds; `0.0` until mpv reports one.
    pub duration: f64,
    pub volume: i64,
    pub speed: f64,
    pub semitones: i32,
    pub reverb: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    pub linked: bool,
    pub remote_linked: bool,
    pub listen_together_role: String,
}

impl StatusResponse {
    pub fn from_snapshot(mut snap: Value, remote_linked: bool, role: String) -> Self {
        if let Some(object) = snap.as_object_mut() {
            object.insert("linked".into(), serde_json::json!(remote_linked));
            object.insert("remote_linked".into(), serde_json::json!(remote_linked));
            object.insert("listen_together_role".into(), serde_json::json!(role));
        }
        serde_json::from_value(snap).unwrap_or_default()
    }
}

/// `GET /api/v1/now-playing`. The current track and the few facts a player bar needs.
///
/// `now` is the same object as in [`StatusResponse`], including `queued_by` when present.
/// `position` and `duration` are **seconds**; `live` is always present here (default `false`).
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NowPlayingResponse {
    pub now: Value,
    pub paused: bool,
    /// Playback position in seconds.
    pub position: f64,
    /// Track length in seconds.
    pub duration: f64,
    pub live: bool,
}

impl NowPlayingResponse {
    pub fn from_snapshot(snap: &Value) -> Self {
        let value = serde_json::json!({
            "now": snap.get("now").cloned().unwrap_or(Value::Null),
            "paused": snap.get("paused").cloned().unwrap_or(serde_json::json!(true)),
            "position": snap.get("position").cloned().unwrap_or(serde_json::json!(0.0)),
            "duration": snap.get("duration").cloned().unwrap_or(serde_json::json!(0.0)),
            "live": snap.get("live").cloned().unwrap_or(serde_json::json!(false)),
        });
        serde_json::from_value(value).unwrap_or_default()
    }
}

/// `GET /api/v1/queue`. A defined projection of the UI queue, not the raw snapshot.
///
/// Item objects are unchanged, including `queued_by` on attributed rows. `current_index` is the
/// zero-based playing row; `next` and `previous` are the whole item objects (or `null`) at
/// `next_index` / `previous_index`. Those indices follow the displayed, possibly shuffled order.
/// `next_is_autoplay_fetch` is true only when `next` is absent because a repeat-off tail would
/// fetch autoplay. `shuffle`, `repeat` (`"off"`, `"all"`, or `"one"`), and `autoplay` are the
/// queue's current modes. No field here has a time unit.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueResponse {
    pub items: Vec<Value>,
    /// Zero-based index of the playing row in `items`.
    pub current_index: usize,
    pub shuffle: bool,
    pub repeat: String,
    pub autoplay: bool,
    pub next: Option<Value>,
    pub previous: Option<Value>,
    pub next_index: Option<usize>,
    pub previous_index: Option<usize>,
    pub next_is_autoplay_fetch: bool,
}

impl QueueResponse {
    pub fn from_snapshot(mut snap: Value, neighbors: QueueNeighbors) -> Self {
        if let Some(object) = snap.as_object_mut() {
            object.insert(
                "current_index".into(),
                object.get("currentIndex").cloned().unwrap_or(serde_json::json!(0)),
            );
            let items = object.get("items").and_then(Value::as_array).cloned();
            let track = |index: Option<usize>| -> Value {
                index
                    .and_then(|i| items.as_ref().and_then(|rows| rows.get(i)))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            object.insert("next".into(), track(neighbors.next));
            object.insert("previous".into(), track(neighbors.previous));
            object.insert("next_index".into(), serde_json::json!(neighbors.next));
            object.insert("previous_index".into(), serde_json::json!(neighbors.previous));
            object.insert(
                "next_is_autoplay_fetch".into(),
                serde_json::json!(neighbors.next_is_autoplay_fetch),
            );
        }
        serde_json::from_value(snap).unwrap_or_default()
    }
}

/// `POST /api/v1/playback/seek` body. `position` is seconds from the start of the track.
#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    pub position: f64,
}

/// `POST /api/v1/playback/volume` body. `volume` is an integer 0–100.
#[derive(Debug, Deserialize)]
pub struct VolumeRequest {
    pub volume: i64,
}

/// `POST /api/v1/queue/remove` body. `index` is zero-based in the displayed queue. When
/// `video_id` is supplied, the row must still match or the request is refused with `queue_changed`.
#[derive(Debug, Deserialize)]
pub struct RemoveRequest {
    pub index: usize,
    #[serde(default)]
    pub video_id: Option<String>,
}

/// Final transport state returned by Play, Pause, and Toggle.
#[derive(Debug, Serialize)]
pub struct PlaybackStateResponse {
    pub playing: bool,
}

/// Result of Previous. `restarted` is true when the three-second rewind rule fired instead of
/// stepping to another row; the queue's `previous` field is independent of this rule.
#[derive(Debug, Serialize)]
pub struct PreviousResponse {
    pub ok: bool,
    pub restarted: bool,
}

/// Effective seek target in seconds, echoed after validation and any clamp applied by the player.
#[derive(Debug, Serialize)]
pub struct SeekResponse {
    /// Effective playback position in seconds.
    pub position: f64,
}

/// Applied volume, an integer 0–100.
#[derive(Debug, Serialize)]
pub struct VolumeResponse {
    pub volume: i64,
}

/// Generic success body for queue edits.
#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

/// Body for `POST /api/v1/queue/request`. `input` may be a YouTube URL, an 11-character video id,
/// or a text query. `requested_by` is stored as `SongItem.queued_by`. See [`ResolveError`] and the
/// response contract on [`QueueRequestResponse`].
#[derive(Debug, Deserialize)]
pub struct QueueRequest {
    pub input: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    /// Maximum accepted track length in whole seconds.
    #[serde(default)]
    pub max_duration_sec: Option<u64>,
    /// Skip this request's duplicate rejection. The app's global `prevent_duplicates` setting still
    /// applies inside the queue insertion.
    #[serde(default)]
    pub allow_duplicates: bool,
    /// Insert at the Play-next boundary instead of before autoplay filler at the tail.
    #[serde(default)]
    pub play_next: bool,
    /// Accept a track whose duration is missing or unparseable when `max_duration_sec` is set.
    #[serde(default)]
    pub allow_unknown_duration: bool,
}

/// Thin variant body for `/queue/add` and `/queue/play-next`; all other options take their defaults.
#[derive(Debug, Deserialize)]
pub struct SimpleQueueRequest {
    pub input: String,
    #[serde(default)]
    pub requested_by: Option<String>,
}

/// Success body for all three queue-request endpoints. `position` is the final zero-based queue
/// index of the inserted row, including 0 when an empty queue starts playback. `item` is the full
/// `SongItem` actually queued, so it carries `duration` and `queued_by` without a second lookup.
#[derive(Debug, Serialize)]
pub struct QueueRequestResponse {
    pub ok: bool,
    pub position: usize,
    pub item: Value,
}

/// Resolution failures that are not plain parser errors. Parser errors are mapped directly from
/// [`crate::integrations::parse::ParseError`] in the handler.
#[derive(Debug, Clone)]
pub enum ResolveError {
    NotFound,
    ResolveFailed(String),
    /// A live broadcast has no normal duration.
    LiveStream,
}

/// Everything handlers need from the app. Implemented by `AppState` in production and by a small
/// mock in tests, which keeps router tests independent of a Tauri handle.
#[async_trait::async_trait]
pub trait ControlApiContext: Send + Sync + 'static {
    /// Same shape the UI's `playback-snapshot` event uses: `now`, `paused`, `position`,
    /// `duration`, `volume`, `speed`, `semitones`, `reverb`, and optional `live`.
    async fn playback_snapshot(&self) -> serde_json::Value;

    /// Same shape the UI's `queue-changed` event uses, including `items[].queued_by`.
    async fn queue_snapshot(&self) -> serde_json::Value;

    /// Read-only next/previous indices, computed from the same snapshot shape.
    async fn queue_neighbors(&self) -> QueueNeighbors;

    /// True when another linked device is the player and local mutations must be refused.
    fn remote_linked(&self) -> bool;

    /// `"none"`, `"host"`, or `"guest"`, matching the UI's Listen Together status.
    async fn listen_together_role(&self) -> String;

    /// True when a Listen Together guest session owns playback policy.
    async fn is_guest(&self) -> bool;

    /// Idempotent Play; starts a restored queue when mpv is idle. Returns the resulting state.
    async fn play(self: Arc<Self>) -> bool;

    /// Idempotent Pause; shared with the Tauri pause command. Returns the resulting state.
    async fn pause(self: Arc<Self>) -> bool;

    /// Toggle transport state and report the final state.
    async fn toggle(self: Arc<Self>) -> bool;

    /// Skip and return the new current queue item, or `None` on timeout/failure. A timeout does
    /// not guarantee the skip did not complete; callers should re-read `/status` before retrying.
    async fn next(self: Arc<Self>) -> Option<Value>;

    /// Step back; returns whether the current track was rewound.
    async fn previous(self: Arc<Self>) -> bool;

    /// Apply a validated seek and return the effective position in seconds.
    async fn seek(self: Arc<Self>, position: f64) -> Result<f64, String>;

    /// Apply a validated volume, an integer 0–100.
    async fn set_volume(self: Arc<Self>, volume: i64) -> Result<(), String>;

    /// Number of rows in the displayed queue, for index validation.
    async fn queue_len(self: Arc<Self>) -> usize;

    /// Remove an upcoming queue row through the existing app path.
    async fn remove_at(self: Arc<Self>, index: usize);

    /// Clear manually queued rows through the existing app path.
    async fn clear_queued(self: Arc<Self>);

    /// Resolve a parsed input to one fully-described song, including duration.
    async fn resolve_input(&self, parsed: &ParsedInput) -> Result<SongItem, ResolveError>;

    /// Check duplicates and insert under one queue lock. `Ok` is the final index; `Err` is the
    /// index of the existing copy. `allow_duplicates` skips only the API-level check.
    async fn insert_request(
        self: Arc<Self>,
        item: SongItem,
        next: bool,
        allow_duplicates: bool,
    ) -> Result<usize, usize>;
}

#[async_trait::async_trait]
impl ControlApiContext for crate::state::AppState {
    async fn playback_snapshot(&self) -> serde_json::Value {
        crate::state::AppState::playback_snapshot(self).await
    }

    async fn queue_snapshot(&self) -> serde_json::Value {
        crate::state::AppState::queue_snapshot(self).await
    }

    async fn queue_neighbors(&self) -> QueueNeighbors {
        self.queue_navigation().await
    }

    fn remote_linked(&self) -> bool {
        crate::state::AppState::remote_linked(self)
    }

    async fn listen_together_role(&self) -> String {
        self.lt_role().await
    }

    async fn is_guest(&self) -> bool {
        self.lt.is_guest().await
    }

    async fn play(self: Arc<Self>) -> bool {
        self.play_playback().await
    }

    async fn pause(self: Arc<Self>) -> bool {
        self.pause_playback().await
    }

    async fn toggle(self: Arc<Self>) -> bool {
        self.resume_or_toggle().await
    }

    async fn next(self: Arc<Self>) -> Option<Value> {
        self.next_with_current().await
    }

    async fn previous(self: Arc<Self>) -> bool {
        self.previous_with_restart().await
    }

    async fn seek(self: Arc<Self>, position: f64) -> Result<f64, String> {
        self.user_seek(position).await?;
        Ok(position.max(0.0))
    }

    async fn set_volume(self: Arc<Self>, volume: i64) -> Result<(), String> {
        self.set_volume_with_effects(volume)
    }

    async fn queue_len(self: Arc<Self>) -> usize {
        crate::state::AppState::queue_len(self.as_ref()).await
    }

    async fn remove_at(self: Arc<Self>, index: usize) {
        self.remove_from_queue(index).await;
    }

    async fn clear_queued(self: Arc<Self>) {
        crate::state::AppState::clear_queued(&self).await;
    }

    async fn resolve_input(&self, parsed: &ParsedInput) -> Result<SongItem, ResolveError> {
        self.resolve_control_input(parsed).await
    }

    async fn insert_request(
        self: Arc<Self>,
        item: SongItem,
        next: bool,
        allow_duplicates: bool,
    ) -> Result<usize, usize> {
        self.insert_queued_checked(item, next, None, allow_duplicates).await
    }
}

/// Router state. The token lives behind a shared lock so regeneration is visible to in-flight
/// requests without restarting the server; the server is still restarted to drop old WS clients.
pub struct ApiShared {
    pub token: Arc<RwLock<String>>,
    pub context: Arc<dyn ControlApiContext>,
    /// Serializes current-track/queue mutations so chat-driven callers see each other's results.
    pub mutation: Arc<Mutex<()>>,
}
