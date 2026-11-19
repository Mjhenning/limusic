//! Shared request state and response shapes for the local control API.

use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
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
