//! Shared request state and response shapes for the local control API.

use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::state::QueueNeighbors;

/// The standard error envelope. `error` is a stable machine code; `message` is human-readable.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: &'static str,
    pub message: &'static str,
}

impl ErrorBody {
    pub fn unauthorized() -> Self {
        Self {
            error: "unauthorized",
            message: "a valid Authorization: Bearer <token> header is required",
        }
    }
}

impl IntoResponse for ErrorBody {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, Json(self)).into_response()
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
}

#[async_trait::async_trait]
impl ControlApiContext for crate::state::AppState {
    async fn playback_snapshot(&self) -> serde_json::Value {
        self.playback_snapshot().await
    }

    async fn queue_snapshot(&self) -> serde_json::Value {
        self.queue_snapshot().await
    }

    async fn queue_neighbors(&self) -> QueueNeighbors {
        self.queue_navigation().await
    }

    fn remote_linked(&self) -> bool {
        self.remote_linked()
    }

    async fn listen_together_role(&self) -> String {
        self.lt_role().await
    }
}

/// Router state. The token lives behind a shared lock so regeneration is visible to in-flight
/// requests without restarting the server; the server is still restarted to drop old WS clients.
pub struct ApiShared {
    pub token: Arc<RwLock<String>>,
    pub context: Arc<dyn ControlApiContext>,
}
