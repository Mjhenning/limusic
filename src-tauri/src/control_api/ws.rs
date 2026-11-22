//! Read-only WebSocket transport for the local control API.
//!
//! Clients upgrade `GET /api/v1/ws` with the same bearer header as HTTP. Each connection owns one
//! hub subscriber and one ~1 Hz timer; a slow consumer is dropped after a per-send timeout, so
//! neither the hub nor playback ever waits on a socket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use futures_util::{Sink, SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio::time::{interval, timeout, Duration, MissedTickBehavior};

use super::handlers::bearer_authorized;
use super::types::{ws_snapshot, ApiShared, ApiTrack, ErrorBody, WsMessage};
use crate::integrations::hub::HubEvent;

/// Hard cap on simultaneous control-API WebSockets. Extra upgrades are refused with JSON, not
/// silently accepted and then starved.
pub const MAX_WS_CLIENTS: usize = 8;

const WS_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
const WS_POSITION_INTERVAL: Duration = Duration::from_secs(1);

/// Shared cap and shutdown signal. The count is decremented by [`WsPermit`] on drop, so an abrupt
/// client disconnect cannot leak a slot.
pub struct WsConnections {
    count: AtomicUsize,
    close: broadcast::Sender<()>,
}

impl Default for WsConnections {
    fn default() -> Self {
        let (close, _) = broadcast::channel(16);
        Self { count: AtomicUsize::new(0), close }
    }
}

impl WsConnections {
    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Reserve one slot and subscribe to the next lifecycle shutdown signal.
    pub fn acquire(self: &Arc<Self>) -> Result<(WsPermit, broadcast::Receiver<()>), ErrorBody> {
        loop {
            let current = self.count.load(Ordering::SeqCst);
            if current >= MAX_WS_CLIENTS {
                return Err(ErrorBody::too_many_clients());
            }
            if self
                .count
                .compare_exchange_weak(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        Ok((WsPermit(Arc::clone(self)), self.close.subscribe()))
    }

    /// Used by token regeneration, disable, rebind and app exit. Every connection sees this and
    /// sends a close frame before its task ends.
    pub fn close_all(&self) {
        let _ = self.close.send(());
    }
}

/// Holds one reserved socket slot until the connection task ends.
pub struct WsPermit(Arc<WsConnections>);

impl Drop for WsPermit {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `GET /api/v1/ws`. The bearer check runs before `on_upgrade`, so a rejected client gets the same
/// 401 JSON envelope as a rejected HTTP request. The query string is never consulted for a token.
pub async fn ws_handler(
    State(shared): State<Arc<ApiShared>>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = bearer_authorized(&shared, &headers).await {
        return error.into_response();
    }
    // Reserve before the upgrade so a ninth client gets the JSON 503, not a 101 it cannot use.
    let (permit, close_rx) = match shared.ws.acquire() {
        Ok(reserved) => reserved,
        Err(error) => return error.into_response(),
    };
    ws.on_upgrade(move |socket| run_socket(socket, shared, permit, close_rx))
}

async fn run_socket(
    socket: WebSocket,
    shared: Arc<ApiShared>,
    permit: WsPermit,
    mut close_rx: broadcast::Receiver<()>,
) {
    let _permit = permit;
    let (mut sink, mut stream) = socket.split();
    let mut hub_rx = shared.hub.subscribe();
    let mut position_tick = interval(WS_POSITION_INTERVAL);
    let mut ping_tick = interval(WS_PING_INTERVAL);
    position_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ping_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // `interval` yields immediately; consume that tick so the first real wait is a full interval.
    position_tick.tick().await;
    ping_tick.tick().await;

    let snapshot = ws_snapshot(&shared).await;
    if !send_json(&mut sink, &WsMessage::Hello { snapshot }).await {
        return;
    }

    loop {
        tokio::select! {
            _ = close_rx.recv() => {
                let _ = send(
                    &mut sink,
                    Message::Close(Some(CloseFrame {
                        code: 1001,
                        reason: "control API restarting".into(),
                    })),
                )
                .await;
                break;
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {
                        // Read-only stream: pongs are handled by tungstenite, commands ignored.
                    }
                    Some(Err(_)) => break,
                }
            }
            event = hub_rx.recv() => {
                match event {
                    Ok(event) => {
                        if let Some(message) = ws_message(event, &shared).await {
                            if !send_json(&mut sink, &message).await {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = ws_snapshot(&shared).await;
                        if !send_json(&mut sink, &WsMessage::Resync { snapshot }).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = position_tick.tick() => {
                let snap = shared.context.playback_snapshot().await;
                if snap.get("paused").and_then(serde_json::Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let position_sec =
                    snap.get("position").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                if !send_json(&mut sink, &WsMessage::Position { position_sec }).await {
                    break;
                }
            }
            _ = ping_tick.tick() => {
                if !send(&mut sink, Message::Ping(Vec::new().into())).await {
                    break;
                }
            }
        }
    }
}

/// Translate one hub event into a wire message. Continuous position ticks are never mirrored;
/// the per-connection timer reads them from the snapshot instead.
async fn ws_message(event: HubEvent, shared: &ApiShared) -> Option<WsMessage> {
    match event {
        HubEvent::NowPlaying(now) => Some(WsMessage::NowPlaying {
            current: ApiTrack::from_value(&now),
            position_sec: now.get("position").and_then(serde_json::Value::as_f64),
            duration_sec: now.get("duration").and_then(serde_json::Value::as_f64),
        }),
        HubEvent::PlaybackState(state) => {
            Some(WsMessage::PlaybackState { playing: state == "playing", state })
        }
        HubEvent::QueueChanged { current } => Some(WsMessage::QueueChanged {
            current_index: current,
            total: Arc::clone(&shared.context).queue_len().await,
        }),
        HubEvent::ShuffleChanged(shuffle) => Some(WsMessage::ShuffleChanged { shuffle }),
        HubEvent::RepeatChanged(repeat) => Some(WsMessage::RepeatChanged { repeat }),
        HubEvent::Volume(volume) => Some(WsMessage::Volume { volume }),
        HubEvent::PlaybackError { message, video_id } => {
            Some(WsMessage::PlaybackError { message, video_id })
        }
        HubEvent::PositionSet(_)
        | HubEvent::Duration(_)
        | HubEvent::TrackLoading(_)
        | HubEvent::TrackFailed(_) => None,
    }
}

/// One send can block an unresponsive peer. Bound it; on expiry the connection is dropped without
/// ever touching the hub or a playback lock.
async fn send<S>(sink: &mut S, message: Message) -> bool
where
    S: Sink<Message, Error = axum::Error> + Unpin,
{
    matches!(timeout(WS_SEND_TIMEOUT, sink.send(message)).await, Ok(Ok(())))
}

async fn send_json<S>(sink: &mut S, message: &WsMessage) -> bool
where
    S: Sink<Message, Error = axum::Error> + Unpin,
{
    let Ok(text) = serde_json::to_string(message) else { return false };
    send(sink, Message::Text(text.into())).await
}
