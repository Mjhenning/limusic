//! Shared request state and response shapes for the local control API.

use std::sync::Arc;

use axum::{
    extract::{FromRequest, Request as AxumRequest},
    http::{header::CONTENT_TYPE, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use innertube::SongItem;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::integrations::parse::ParsedInput;
use crate::state::QueueNeighbors;

/// The existing queue row that made a request a duplicate. `matched_by` names the requested field:
/// `video_id` or `counterpart`. The row is identified by both its id and its twin so a client can
/// see that an audio request collided with a music video's counterpart (or vice versa).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DuplicateMatch {
    pub position: usize,
    pub video_id: String,
    pub counterpart: Option<String>,
    pub matched_by: String,
}

/// Compare one requested item with the full queue, including the current row. Either side's
/// `video_id` or `counterpart` counts, so an audio id and its paired music video are one song.
pub fn find_request_duplicate<'a>(
    item: &SongItem,
    queue: impl IntoIterator<Item = &'a SongItem>,
) -> Option<DuplicateMatch> {
    let requested = [
        ("video_id", Some(item.video_id.as_str())),
        ("counterpart", item.counterpart.as_deref().filter(|id| !id.is_empty())),
    ];
    let queue: Vec<&SongItem> = queue.into_iter().collect();
    for (matched_by, wanted) in requested {
        let Some(wanted) = wanted else { continue };
        for (position, row) in queue.iter().copied().enumerate() {
            let row_counterpart = row.counterpart.as_deref().filter(|id| !id.is_empty());
            if row.video_id == wanted || row_counterpart == Some(wanted) {
                return Some(DuplicateMatch {
                    position,
                    video_id: row.video_id.clone(),
                    counterpart: row_counterpart.map(str::to_owned),
                    matched_by: matched_by.to_owned(),
                });
            }
        }
    }
    None
}

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
    /// The row that matched, on a `duplicate` rejection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<DuplicateMatch>,
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
            matched: None,
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

    pub fn duplicate(matched: DuplicateMatch) -> Self {
        let mut error = Self::new(
            StatusCode::CONFLICT,
            "duplicate",
            "that song is already in the queue (counterparts count as the same song)",
        );
        error.position = Some(matched.position);
        error.matched = Some(matched);
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

    pub fn explicit_content() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "explicit_content",
            "the resolved track is explicit and reject_explicit is set",
        )
    }

    pub fn playback_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "playback_error", message)
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
    }

    pub fn unsupported_media_type() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "request bodies must use application/json",
        )
    }

    pub fn route_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no such API route")
    }

    pub fn method_not_allowed() -> Self {
        Self::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "method not allowed")
    }
}

impl IntoResponse for ErrorBody {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// JSON body extractor that puts every failure in the API's error envelope. `serde_path_to_error`
/// names the offending field, so a missing or mistyped field gets a useful 400 `invalid_input`
/// instead of axum's plain-text 422. A non-JSON `Content-Type` gets 415.
#[derive(Debug, Clone, Copy)]
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ErrorBody;

    fn from_request(
        req: AxumRequest,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        async move {
            let is_json =
                req.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_some_and(
                    |value| value.to_ascii_lowercase().starts_with("application/json"),
                );
            if !is_json {
                return Err(ErrorBody::unsupported_media_type());
            }
            let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .map_err(|_| ErrorBody::invalid_input("could not read the JSON body"))?;
            let value = serde_path_to_error::deserialize::<_, T>(
                &mut serde_json::Deserializer::from_slice(&bytes),
            )
            .map_err(|error| {
                let path = error.path().to_string();
                let field = path.trim_start_matches('.').split('.').next().unwrap_or("body");
                ErrorBody::invalid_input(format!(
                    "invalid field '{}': {}",
                    if field.is_empty() { "body" } else { field },
                    error.inner()
                ))
            })?;
            Ok(Self(value))
        }
    }
}

/// Query extractor that keeps malformed query strings in the error envelope too.
#[derive(Debug, Clone, Copy)]
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequest<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ErrorBody;

    fn from_request(
        req: AxumRequest,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        async move {
            let query = req.uri().query().unwrap_or("");
            let value = serde_urlencoded::from_str(query)
                .map_err(|_| ErrorBody::invalid_input("invalid query string"))?;
            Ok(Self(value))
        }
    }
}

/// Parse a `SongItem.duration` string (`m:ss` / `h:m:ss`) into whole seconds. `None` means the
/// metadata gave no usable length, which is the `duration_unknown` case.
pub fn duration_secs(text: Option<&str>) -> Option<u64> {
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

/// Per-artist names that link to an artist page. Separator runs (`", "`, `" & "`) have no id and
/// are skipped; when the row carries no linked runs the display string is the only safe array.
pub fn artist_list(item: &SongItem) -> Vec<String> {
    let linked: Vec<String> = item
        .artist_runs
        .iter()
        .filter(|run| run.id.as_deref().is_some_and(|id| !id.is_empty()))
        .map(|run| run.text.clone())
        .collect();
    if linked.is_empty() {
        vec![item.artists.clone()]
    } else {
        linked
    }
}

/// The stable, slim track shape exposed by the control API. Optional metadata may be `null` — a
/// URL-resolved row often has no album or thumbnail — but library tokens, play count, rating and
/// added_at are never exposed. `duration_sec` is parsed from `duration` for convenience.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApiTrack {
    pub video_id: String,
    pub title: String,
    pub artists: Option<String>,
    pub artist_list: Vec<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub duration: Option<String>,
    pub duration_sec: Option<u64>,
    pub thumbnail: Option<String>,
    pub explicit: bool,
    pub is_video: bool,
    pub live: bool,
    pub queued_by: Option<String>,
    pub queued: bool,
    pub queued_end: bool,
}

impl ApiTrack {
    pub fn from_song_item(item: &SongItem, live: bool) -> Self {
        Self {
            video_id: item.video_id.clone(),
            title: item.title.clone(),
            artists: Some(item.artists.clone()),
            artist_list: artist_list(item),
            album: item.album.clone(),
            album_id: item.album_id.clone(),
            duration: item.duration.clone(),
            duration_sec: duration_secs(item.duration.as_deref()),
            thumbnail: item.thumbnail.clone(),
            explicit: item.explicit,
            is_video: item.is_video,
            live,
            queued_by: item.queued_by.clone(),
            queued: item.queued,
            queued_end: item.queued_end,
        }
    }

    /// Accepts both the app's snake_case `SongItem` rows and older `videoId` shaped snapshots from
    /// tests and remote-device payloads. Missing fields stay `null`/empty rather than failing.
    pub fn from_value(value: &Value) -> Self {
        if let Ok(item) = serde_json::from_value::<SongItem>(value.clone()) {
            let live = value.get("live").and_then(Value::as_bool).unwrap_or(false);
            return Self::from_song_item(&item, live);
        }
        let get = |snake: &str, camel: &str| field(value, snake, camel);
        let duration = get("duration", "duration").and_then(Value::as_str).map(str::to_owned);
        let artists = get("artists", "artists").and_then(Value::as_str).map(str::to_owned);
        let artist_list = get("artist_list", "artistList")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_else(|| artists.clone().into_iter().collect());
        Self {
            video_id: get("video_id", "videoId").and_then(Value::as_str).unwrap_or_default().into(),
            title: get("title", "title").and_then(Value::as_str).unwrap_or_default().into(),
            artists,
            artist_list,
            album: get("album", "album").and_then(Value::as_str).map(str::to_owned),
            album_id: get("album_id", "albumId").and_then(Value::as_str).map(str::to_owned),
            duration_sec: get("duration_sec", "durationSec")
                .and_then(Value::as_u64)
                .or_else(|| duration_secs(duration.as_deref())),
            duration,
            thumbnail: get("thumbnail", "thumbnail").and_then(Value::as_str).map(str::to_owned),
            explicit: get("explicit", "explicit").and_then(Value::as_bool).unwrap_or(false),
            is_video: get("is_video", "isVideo").and_then(Value::as_bool).unwrap_or(false),
            live: get("live", "live").and_then(Value::as_bool).unwrap_or(false),
            queued_by: get("queued_by", "queuedBy").and_then(Value::as_str).map(str::to_owned),
            queued: get("queued", "queued").and_then(Value::as_bool).unwrap_or(false),
            queued_end: get("queued_end", "queuedEnd").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

fn field<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    value.get(snake).or_else(|| value.get(camel))
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

/// `GET /api/v1/queue`. A defined projection of the displayed queue, not the raw snapshot.
///
/// Items, `next` and `previous` are [`ApiTrack`] values. `current_index`, `next_index` and
/// `previous_index` are zero-based indices into the full queue; `total` is its length before any
/// `limit`/`offset` slicing. `next_is_autoplay_fetch` is true only when `next` is absent because a
/// repeat-off tail would fetch autoplay. No field here has a time unit except `duration_sec` on a
/// track, which is seconds.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueResponse {
    pub items: Vec<ApiTrack>,
    /// Zero-based index of the playing row in the full queue.
    pub current_index: usize,
    pub shuffle: bool,
    pub repeat: String,
    pub autoplay: bool,
    pub next: Option<ApiTrack>,
    pub previous: Option<ApiTrack>,
    pub next_index: Option<usize>,
    pub previous_index: Option<usize>,
    pub next_is_autoplay_fetch: bool,
    pub total: usize,
}

impl QueueResponse {
    pub fn from_snapshot(mut snap: Value, neighbors: QueueNeighbors) -> Self {
        let current_index = snap.get("currentIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
        let raw_items = snap.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let items: Vec<ApiTrack> = raw_items.iter().map(ApiTrack::from_value).collect();
        let track = |index: Option<usize>| -> Option<ApiTrack> {
            index.and_then(|i| items.get(i)).cloned()
        };
        let total = items.len();
        if let Some(object) = snap.as_object_mut() {
            object.insert("items".into(), serde_json::to_value(&items).unwrap_or(Value::Null));
            object.insert("current_index".into(), serde_json::json!(current_index));
            object.insert(
                "next".into(),
                serde_json::to_value(track(neighbors.next)).unwrap_or(Value::Null),
            );
            object.insert(
                "previous".into(),
                serde_json::to_value(track(neighbors.previous)).unwrap_or(Value::Null),
            );
            object.insert("next_index".into(), serde_json::json!(neighbors.next));
            object.insert("previous_index".into(), serde_json::json!(neighbors.previous));
            object.insert(
                "next_is_autoplay_fetch".into(),
                serde_json::json!(neighbors.next_is_autoplay_fetch),
            );
            object.insert("total".into(), serde_json::json!(total));
        }
        serde_json::from_value(snap).unwrap_or_default()
    }

    /// Slice only the page of rows; `total` and the navigation fields still describe the full
    /// queue. With no parameters this is a clone of the unmodified response.
    pub fn page(mut self, limit: Option<usize>, offset: Option<usize>) -> Self {
        if limit.is_some() || offset.is_some() {
            let start = offset.unwrap_or(0).min(self.items.len());
            let end =
                limit.map_or(self.items.len(), |n| start.saturating_add(n).min(self.items.len()));
            self.items = self.items[start..end.max(start)].to_vec();
        }
        self
    }
}

/// Optional `limit`/`offset` paging for `GET /api/v1/queue`.
#[derive(Debug, Deserialize)]
pub struct QueueQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// `POST /api/v1/playback/seek` body. `position_sec` is seconds from the start of the track;
/// `position` remains accepted as a serde alias for older clients.
#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    #[serde(alias = "position")]
    pub position_sec: f64,
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

/// Final transport state returned by Play, Pause and Toggle. `playing` is the state after the
/// command; `ok` is always true on success.
#[derive(Debug, Serialize)]
pub struct PlaybackStateResponse {
    pub ok: bool,
    pub playing: bool,
}

/// Result of Next. `current` is the queue row that became current after the skip.
#[derive(Debug, Serialize)]
pub struct NextResponse {
    pub ok: bool,
    pub current: ApiTrack,
}

/// Result of Previous. `restarted` is true when the three-second rewind rule fired instead of
/// stepping to another row; `current` is the row current after the action. The read-only queue
/// `previous` field is independent of this rule.
#[derive(Debug, Serialize)]
pub struct PreviousResponse {
    pub ok: bool,
    pub restarted: bool,
    pub current: ApiTrack,
}

/// Effective seek target in seconds, echoed after validation.
#[derive(Debug, Serialize)]
pub struct SeekResponse {
    pub ok: bool,
    /// Effective playback position in seconds.
    pub position_sec: f64,
}

/// Applied volume, an integer 0–100.
#[derive(Debug, Serialize)]
pub struct VolumeResponse {
    pub ok: bool,
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
    /// Insert at the FIFO Play-next block instead of before autoplay filler at the tail. Omitted on
    /// `/queue/request` means true; the thin `/queue/add` variant sets false explicitly.
    #[serde(default)]
    pub play_next: Option<bool>,
    /// Accept a track whose duration is missing or unparseable when `max_duration_sec` is set.
    #[serde(default)]
    pub allow_unknown_duration: bool,
    /// Refuse explicit tracks with 422 `explicit_content`.
    #[serde(default)]
    pub reject_explicit: bool,
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
    pub item: ApiTrack,
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
    /// not guarantee the skip failed; callers should re-read `/status` before retrying.
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

    /// Check duplicates and insert under one queue lock. `Ok` is the final index and the row as it
    /// exists after insertion (so callers see `queued`/`queued_end`); `Err` names the existing row,
    /// including a counterpart match. `allow_duplicates` skips only the API-level check.
    async fn insert_request(
        self: Arc<Self>,
        item: SongItem,
        next: bool,
        allow_duplicates: bool,
    ) -> Result<(usize, SongItem), DuplicateMatch>;
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
    ) -> Result<(usize, SongItem), DuplicateMatch> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use innertube::models::metadata::ArtistRun;

    #[test]
    fn duration_and_artist_helpers_cover_display_metadata() {
        assert_eq!(duration_secs(Some("3:04")), Some(184));
        assert_eq!(duration_secs(Some("1:02:03")), Some(3723));
        assert_eq!(duration_secs(Some("nope")), None);
        assert_eq!(duration_secs(None), None);

        let item = SongItem {
            artists: "A & B".into(),
            artist_runs: vec![
                ArtistRun { text: "A".into(), id: Some("a".into()) },
                ArtistRun { text: " & ".into(), id: None },
                ArtistRun { text: "B".into(), id: Some("b".into()) },
            ],
            ..Default::default()
        };
        assert_eq!(artist_list(&item), vec!["A", "B"]);
        assert_eq!(artist_list(&SongItem::default()), vec![""]);
    }
}
