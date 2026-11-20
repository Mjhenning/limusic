//! Internal mirror of the events shown in the UI.
//!
//! This is deliberately *not* a second playback source. WS/Tuna consumers subscribe to the
//! mirror and fetch authoritative state through `AppState`. Continuous mpv position ticks are
//! excluded; consumers keep their own ~1s timer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HubEvent {
    NowPlaying(serde_json::Value),
    PlaybackState(String),
    /// Discrete position updates: seek, pause snapshot, track start. Not mpv ticks.
    PositionSet(f64),
    Duration(f64),
    /// `queue-changed`, `queue-index` and `queue-appended` all collapse into this. Consumers
    /// fetch `queue_snapshot()` when they need the rows.
    QueueChanged {
        current: Option<usize>,
    },
    ShuffleChanged(bool),
    RepeatChanged(String),
    Volume(i64),
    TrackLoading(String),
    TrackFailed(String),
    PlaybackError(String),
}

impl HubEvent {
    /// Map the UI event names this app actually emits into hub events. Returns `None` for UI
    /// events the integrations do not consume and for mpv position ticks.
    pub fn from_ui_event(event: &str, payload: &serde_json::Value, hub: bool) -> Option<Self> {
        if !hub {
            return None;
        }
        match event {
            "now-playing" => Some(Self::NowPlaying(payload.clone())),
            "playback-state" => payload.as_str().map(|s| Self::PlaybackState(s.to_string())),
            "position" => {
                payload.get("position").and_then(serde_json::Value::as_f64).map(Self::PositionSet)
            }
            "duration" => {
                payload.get("duration").and_then(serde_json::Value::as_f64).map(Self::Duration)
            }
            "queue-changed" | "queue-index" | "queue-appended" => Some(Self::QueueChanged {
                current: payload
                    .get("currentIndex")
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as usize),
            }),
            "volume" => payload.as_i64().map(Self::Volume),
            "track-loading" => payload.as_str().map(|s| Self::TrackLoading(s.to_string())),
            "track-failed" => payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(|s| Self::TrackFailed(s.to_string())),
            "playback-error" => payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(|s| Self::PlaybackError(s.to_string())),
            _ => None,
        }
    }
}

pub fn spawn_debug_logger(mut rx: tokio::sync::broadcast::Receiver<HubEvent>) {
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => tracing::debug!(?event, "ui hub event"),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "ui hub subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ui_events() {
        let pos = serde_json::json!({ "position": 12.5 });
        assert!(matches!(
            HubEvent::from_ui_event("position", &pos, true),
            Some(HubEvent::PositionSet(12.5))
        ));
        assert!(HubEvent::from_ui_event("position", &pos, false).is_none());
        assert!(matches!(
            HubEvent::from_ui_event(
                "queue-changed",
                &serde_json::json!({ "currentIndex": 3 }),
                true
            ),
            Some(HubEvent::QueueChanged { current: Some(3) })
        ));
    }

    #[test]
    fn now_playing_keeps_queued_by() {
        let payload = serde_json::json!({ "videoId": "abc", "queued_by": "friend" });
        match HubEvent::from_ui_event("now-playing", &payload, true) {
            Some(HubEvent::NowPlaying(now)) => assert_eq!(now["queued_by"], "friend"),
            other => panic!("expected now-playing, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unrelated_events() {
        assert!(HubEvent::from_ui_event("karaoke", &serde_json::json!({}), true).is_none());
    }
}
