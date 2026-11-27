//! Listen Together client. Owns the WebSocket connection to a self-hosted `limusic-sync` server
//! (context/19), the room state, and reconnection. It knows nothing about mpv or the orchestrator:
//! playback for guests is driven through an mpsc channel of [`SyncCommand`]s that a bridge task
//! (in `lib.rs`) applies to `AppState`. The host's playback is broadcast by `AppState` calling
//! [`LtSession::broadcast_playback`].
//!
//! No back-reference to `AppState` (avoids an Arc cycle): everything the connection needs is the
//! `AppHandle` (to emit UI events) and the sync channel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use listen_protocol::{
    ClientMessage, Playback, PlaybackKind, RemoteAction, RoomState, ServerMessage,
    SharedControlEnvelope, SharedControlFlags, SharedControlPeer, Suggestion, Track, User,
};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Playback intents the connection hands to the bridge task to apply to the local player.
#[derive(Debug, Clone)]
pub enum SyncCommand {
    /// Full state (after join / reconnect / request-sync): resolve current track, seek to live
    /// position, set play/pause, mirror the queue.
    ApplyState(RoomState),
    /// `track` is `None` when the host is on something a guest can't play (a local file).
    ChangeTrack {
        track: Option<Track>,
        position_ms: i64,
        playing: bool,
        queue: Vec<Track>,
    },
    Play {
        position_ms: i64,
    },
    Pause {
        position_ms: i64,
    },
    Seek {
        position_ms: i64,
    },
    /// Guest: mirror the host's upcoming queue (a guest added/the host removed a track).
    SyncQueue {
        queue: Vec<Track>,
    },
    /// Host: a guest's track to insert at the guest boundary (auto-approved suggestion,
    /// `queued_by` already stamped with the guest's name).
    GuestAdd {
        track: Track,
    },
    /// Host-initiated queue flags carried with a playback broadcast. `None` fields are ignored so
    /// an older host never resets a guest that already has its own flags.
    QueueOptions {
        shuffle: Option<bool>,
        repeat: Option<String>,
    },
    /// Host: an opted-in guest asked this player to apply a control. The bridge applies it through
    /// `AppState::apply_link` and sends the correlated result back to `from`.
    SharedControlRequest {
        id: String,
        from: String,
        action: RemoteAction,
        submitted_by: Option<String>,
    },
    /// We just became host of a freshly-created room — seed the room with our current now-playing.
    HostSeed,
    /// We left / were kicked / became host — stop applying remote playback.
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    None,
    Host,
    Guest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Disconnected,
    Connecting,
    Connected,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Disconnected => "disconnected",
            Status::Connecting => "connecting",
            Status::Connected => "connected",
        }
    }
}

/// The server a user gets when they have set none of their own. Kept out of the UI on purpose:
/// the settings field seeds empty, `snapshot_of` reports it empty, and an invite minted on it is a
/// bare room code, so nobody has to see (or retype) somebody else's hostname to listen together.
/// An empty `server_url` means "this one", resolved at connect time in [`LtSession::run`], so
/// clearing the field is how a self-hoster comes back to the default.
pub(crate) const DEFAULT_SERVER: &str = "wss://sync.limusic.app/ws";

/// How long a guest's local Control API waits for the host's correlated answer. Matches the HTTP
/// side's longest mutation deadline: a request that outlives this answers `shared_control_failed`
/// instead of leaving the caller waiting forever.
pub(crate) const SHARED_CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// The previous default, a Tailscale Funnel address some ISP filters reset (#627). Builds that
/// pre-filled the server field saved it into the setting and minted it into invites, so it still
/// arrives from both. It means the default, so it is stored as empty and moves with
/// [`DEFAULT_SERVER`].
const OLD_DEFAULT_SERVER: &str = "wss://fedora-1.tail9c4985.ts.net/ws";

pub(crate) fn stored_url(url: String) -> String {
    if url.trim() == OLD_DEFAULT_SERVER {
        String::new()
    } else {
        url
    }
}

/// The hidden device-link room is keyed by the room code itself, so the invite stays a bare code
/// on the default relay. The digest is 32 bytes → 43 base64url chars, which upstream accepts as a
/// link secret; the short hex prefix gives the guest a stable device id to target on that link.
/// Anyone who has the room code can already join the room, so this adds no new capability leak.
fn shared_link_keys(room_code: &str) -> (String, String) {
    let digest = Sha256::digest(format!("limusic-shared-control|{room_code}").as_bytes());
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    let host_suffix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    (secret, format!("sc-host-{host_suffix}"))
}

/// Guests use a stable hidden-link device id derived from their room user id. That lets the host
/// attribute a persistent socket to the real participant, and lets the guest's own tag follow its
/// settings without opening a second socket. Older per-request ids had a random suffix; accept it
/// too so a host that upgrades mid-room can still badge those guests.
fn linked_guest_id(device_id: &str) -> Option<String> {
    let rest = device_id.strip_prefix("sc-guest-")?;
    let id = rest.split_once('-').map_or(rest, |(id, _)| id);
    (!id.is_empty()).then(|| id.to_owned())
}

fn upsert_peer(peers: &mut Vec<SharedControlPeer>, peer: SharedControlPeer) {
    match peers.iter_mut().find(|p| p.user_id == peer.user_id) {
        Some(existing) => *existing = peer,
        None => peers.push(peer),
    }
}

fn peer_for_user(inner: &Inner, user_id: &str) -> SharedControlPeer {
    let username = inner
        .users
        .iter()
        .find(|u| u.user_id == user_id)
        .map(|u| u.username.clone())
        .unwrap_or_else(|| user_id.to_string());
    SharedControlPeer {
        user_id: user_id.to_string(),
        username,
        api_enabled: false,
        ui_control: false,
    }
}

/// Map relay errors to the same diagnostics the dialog and the guest HTTP error show.
fn shared_link_error(code: &str, message: &str) -> String {
    match code {
        "no_device" => "the host is not allowing shared control for this room".into(),
        "room_full" => {
            "too many guests are using shared control in this room; retry shortly".into()
        }
        "bad_secret" => "the room code could not open the shared-control channel".into(),
        "server_full" => "the Listen Together server is full; retry shortly".into(),
        _ => message.to_string(),
    }
}

#[derive(Default)]
struct Inner {
    status_connected: bool,
    connecting: bool,
    /// We've asked to create/join and are waiting for the room to materialize (host approval).
    /// Drives the guest's "waiting" UI and blocks duplicate requests.
    requesting: bool,
    role: Role,
    server_url: String,
    room_code: Option<String>,
    my_id: Option<String>,
    session_token: Option<String>,
    users: Vec<User>,
    current_track: Option<Track>,
    queue: Vec<Track>,
    pending_joins: Vec<(String, String)>, // (user_id, username)
    suggestions: Vec<Suggestion>,
    outbound: Option<mpsc::UnboundedSender<ClientMessage>>,
    /// Persisted host opt-in. Survives leaving rooms so the next room starts with the same policy.
    shared_control_enabled: bool,
    /// What the relay advertises in `RoomState`.
    shared_control_capable: bool,
    /// The current host's opt-in, mirrored from `RoomState`.
    shared_control: bool,
    /// The current host's fork capability, mirrored from `RoomState`.
    host_shared_control_capable: bool,
    /// The hidden link room keyed by this room's code is live. Host-side readiness, and on a guest
    /// whether the host answered the capability handshake.
    shared_link_ready: bool,
    /// Connected devices in that hidden link room, including us when we are the host. The relay
    /// caps a link room at 8 devices, so this is the host's guest-slot counter.
    shared_link_users: usize,
    /// Guest user ids of devices currently on the hidden link, plus their live capabilities.
    /// Replaces the old "seen once" list: leaving or turning both flags off drops the entry.
    shared_link_guests: Vec<SharedControlPeer>,
    /// Hidden-link device ids currently connected, without the host's own id. Authoritative for
    /// slot counting and for routing roster broadcasts.
    shared_link_device_ids: Vec<String>,
    /// The guest's stable device id on this room's hidden link.
    shared_link_local_id: Option<String>,
    /// True when the host answered the hidden-link handshake with `host_allows`.
    shared_link_host_allows: bool,
    /// Local Control API setting, mirrored from the DB so the link can open before the first HTTP
    /// mutation. The UI toggle is independent; either one makes this client capable.
    control_api_enabled: bool,
    /// Desktop player buttons may forward to the host. Persisted and mirrored into the link.
    client_control_enabled: bool,
    /// Last guest-side failure of the hidden link, surfaced in the dialog and `409` message.
    shared_link_reason: Option<String>,
}

impl Default for Role {
    fn default() -> Self {
        Role::None
    }
}

impl Inner {
    fn status(&self) -> Status {
        if self.status_connected {
            Status::Connected
        } else if self.connecting {
            Status::Connecting
        } else {
            Status::Disconnected
        }
    }

    /// Clear all room membership (leave / kicked / fatal). Keeps `server_url`.
    fn reset_room(&mut self) {
        self.role = Role::None;
        self.room_code = None;
        self.my_id = None;
        self.session_token = None;
        self.users.clear();
        self.current_track = None;
        self.queue.clear();
        self.pending_joins.clear();
        self.suggestions.clear();
        self.outbound = None;
        self.shared_control_capable = false;
        self.shared_control = false;
        self.host_shared_control_capable = false;
        self.shared_link_ready = false;
        self.shared_link_users = 0;
        self.shared_link_guests.clear();
        self.shared_link_device_ids.clear();
        self.shared_link_local_id = None;
        self.shared_link_host_allows = false;
        // `control_api_enabled` and `client_control_enabled` are persisted settings: they survive
        // a room change, exactly like `shared_control_enabled`.
        self.shared_link_reason = None;
        self.status_connected = false;
        self.connecting = false;
        self.requesting = false;
    }

    fn absorb(&mut self, state: &RoomState) {
        self.room_code = Some(state.room_code.clone());
        self.users = state.users.clone();
        self.current_track = state.current_track.clone();
        self.queue = state.queue.clone();
        self.shared_control_capable = state.shared_control_capable;
        self.shared_control = state.shared_control;
        self.host_shared_control_capable = state.host_shared_control_capable;
    }
}

pub struct LtSession {
    app: AppHandle,
    inner: Arc<Mutex<Inner>>,
    sync_tx: mpsc::UnboundedSender<SyncCommand>,
    /// Bumped to cancel the running connection loop (leave / new connection).
    gen: AtomicU64,
    /// Monotonic suffix for correlation ids; the generation prefix makes them unique across rooms.
    control_id: AtomicU64,
    /// Bumped to cancel the hidden shared-control link when the room or opt-in changes.
    shared_gen: AtomicU64,
    /// Which room-code-derived link room the host session is responsible for.
    shared_code: Mutex<Option<String>>,
    /// Active hidden-link socket's outbound channel, used only for correlated `control_result`s.
    shared_outbound: Mutex<Option<mpsc::UnboundedSender<ClientMessage>>>,
    /// Bumped to cancel the guest's persistent hidden-link socket (leave / role or room change).
    guest_link_gen: AtomicU64,
    /// Which room-code-derived link room the guest session is responsible for.
    guest_link_code: Mutex<Option<String>>,
    /// Guest hidden-link socket's outbound channel, reused by every request and capability update.
    guest_link_outbound: Mutex<Option<mpsc::UnboundedSender<ClientMessage>>>,
    /// Correlated answers still waiting on the guest's single socket.
    guest_link_pending: Mutex<HashMap<String, oneshot::Sender<Result<SharedControlFlags, String>>>>,
}

impl LtSession {
    /// Create the session. Returns the receiver the bridge task drains to drive guest playback.
    pub fn new(
        app: AppHandle,
        server_url: String,
        shared_control_enabled: bool,
        api_enabled: bool,
        client_control_enabled: bool,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<SyncCommand>) {
        // rustls needs a process-wide crypto provider before the first `wss://` handshake.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let inner = Inner {
            server_url: stored_url(server_url),
            shared_control_enabled,
            control_api_enabled: api_enabled,
            client_control_enabled,
            ..Inner::default()
        };
        let s = Arc::new(LtSession {
            app,
            inner: Arc::new(Mutex::new(inner)),
            sync_tx,
            gen: AtomicU64::new(0),
            control_id: AtomicU64::new(0),
            shared_gen: AtomicU64::new(0),
            shared_code: Mutex::new(None),
            shared_outbound: Mutex::new(None),
            guest_link_gen: AtomicU64::new(0),
            guest_link_code: Mutex::new(None),
            guest_link_outbound: Mutex::new(None),
            guest_link_pending: Mutex::new(HashMap::new()),
        });
        (s, sync_rx)
    }

    // --- public API (called by commands + AppState) -----------------------------------------

    pub async fn set_server_url(&self, url: String) {
        self.inner.lock().await.server_url = stored_url(url);
        self.emit_state().await;
    }

    /// True when we're in a room in any role.
    pub async fn in_room(&self) -> bool {
        self.inner.lock().await.role != Role::None
    }

    /// True when we're a guest in a room — the caller should block local playback control.
    pub async fn is_guest(&self) -> bool {
        self.inner.lock().await.role == Role::Guest
    }

    pub async fn is_host(&self) -> bool {
        self.inner.lock().await.role == Role::Host
    }

    /// Persist the host opt-in. The hidden link room is keyed by the room code, so this can be
    /// flipped live while a room is active: turning it on opens the link, turning it off closes it.
    pub async fn set_shared_control_enabled(self: &Arc<Self>, on: bool) {
        let (host, code) = {
            let mut inner = self.inner.lock().await;
            inner.shared_control_enabled = on;
            (inner.role == Role::Host, inner.room_code.clone())
        };
        if host {
            if on {
                self.start_shared_host_link(code).await;
            } else {
                self.stop_shared_host_link().await;
            }
            self.announce_shared_control().await;
        }
        self.emit_state().await;
    }

    /// Mirror the persisted Control API switch so a capable guest can open its link on join,
    /// before the first HTTP mutation arrives. Toggling it updates the host's roster in place.
    pub async fn set_api_enabled(self: &Arc<Self>, on: bool) {
        {
            let mut inner = self.inner.lock().await;
            if inner.control_api_enabled == on {
                return;
            }
            inner.control_api_enabled = on;
        }
        self.sync_guest_capability().await;
    }

    /// Mirror the desktop player-control switch. Independent of the Control API: either one makes
    /// this guest capable, and both markers can appear on the same participant.
    pub async fn set_client_control_enabled(self: &Arc<Self>, on: bool) {
        {
            let mut inner = self.inner.lock().await;
            if inner.client_control_enabled == on {
                return;
            }
            inner.client_control_enabled = on;
        }
        self.sync_guest_capability().await;
    }

    /// Start/stop the guest link when capability changes, or refresh the host's roster if it is
    /// already open. The main room socket is never involved.
    async fn sync_guest_capability(self: &Arc<Self>) {
        let (eligible, guest, code) = {
            let inner = self.inner.lock().await;
            (
                inner.control_api_enabled || inner.client_control_enabled,
                inner.role == Role::Guest,
                inner.room_code.clone(),
            )
        };
        if eligible && guest {
            if let Some(code) = code {
                self.maybe_start_shared_guest_link(Some(code)).await;
                self.send_guest_hello().await;
            }
        } else {
            self.stop_shared_guest_link().await;
        }
        self.emit_state().await;
    }

    /// Legacy relay state announcement. The hidden link does not need it, but a fork relay that
    /// still understands `SetSharedControl` keeps its room state consistent for older clients.
    async fn announce_shared_control(&self) {
        let on = self.inner.lock().await.shared_control_enabled;
        self.send(ClientMessage::Remote {
            action: SharedControlEnvelope::SetSharedControl { on, shared_control_capable: true }
                .to_value(),
            to: None,
        })
        .await;
    }

    /// Start (or keep) the host's hidden link when we are a opted-in host in a room.
    async fn maybe_start_shared_host_link(self: &Arc<Self>) {
        let (enabled, host, code) = {
            let inner = self.inner.lock().await;
            (inner.shared_control_enabled, inner.role == Role::Host, inner.room_code.clone())
        };
        if enabled && host {
            self.start_shared_host_link(code).await;
        }
    }

    async fn start_shared_host_link(self: &Arc<Self>, code: Option<String>) {
        let Some(code) = code else { return };
        {
            let mut active = self.shared_code.lock().await;
            if active.as_deref() == Some(code.as_str()) {
                return;
            }
            *active = Some(code.clone());
        }
        let (secret, host_id) = shared_link_keys(&code);
        let url = {
            let inner = self.inner.lock().await;
            if inner.server_url.is_empty() {
                DEFAULT_SERVER.to_string()
            } else {
                inner.server_url.clone()
            }
        };
        let gen = self.shared_gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.shared_outbound.lock().await = None;
        {
            let mut inner = self.inner.lock().await;
            inner.shared_link_ready = false;
            inner.shared_link_users = 0;
            inner.shared_link_guests.clear();
            inner.shared_link_device_ids.clear();
            inner.shared_link_local_id = None;
            inner.shared_link_host_allows = false;
            inner.shared_link_reason = None;
        }
        let me = self.clone();
        tokio::spawn(async move { me.run_shared_host_link(gen, secret, host_id, url).await });
        self.emit_state().await;
    }

    async fn stop_shared_host_link(&self) {
        self.shared_gen.fetch_add(1, Ordering::SeqCst);
        *self.shared_code.lock().await = None;
        *self.shared_outbound.lock().await = None;
        {
            let mut inner = self.inner.lock().await;
            inner.shared_link_ready = false;
            inner.shared_link_users = 0;
            inner.shared_link_guests.clear();
            inner.shared_link_device_ids.clear();
            inner.shared_link_local_id = None;
            inner.shared_link_host_allows = false;
            inner.shared_link_reason = None;
        }
        self.emit_state().await;
    }

    /// Host side of the hidden link. It joins a device-link room keyed by the room code and turns
    /// only `control_request` envelopes into sync commands; every other device-link message is
    /// ignored. Reconnects independently of the Listen Together socket.
    async fn run_shared_host_link(
        self: Arc<Self>,
        gen: u64,
        secret: String,
        host_id: String,
        url: String,
    ) {
        let mut attempt = 0;
        while self.shared_gen.load(Ordering::SeqCst) == gen {
            let ws = match connect_ws(&url).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::warn!(error = %e, "shared-control link connect failed");
                    attempt += 1;
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }
            };
            attempt = 0;
            let (mut sink, mut read) = ws.split();
            let (otx, mut orx) = mpsc::unbounded_channel::<ClientMessage>();
            *self.shared_outbound.lock().await = Some(otx.clone());
            let _ = otx.send(ClientMessage::JoinLink {
                secret: secret.clone(),
                device_id: host_id.clone(),
                name: "Shared control".into(),
            });
            let mut writer = tokio::spawn(async move {
                while let Some(msg) = orx.recv().await {
                    if sink.send(Message::Text(msg.to_json())).await.is_err() {
                        break;
                    }
                }
            });

            loop {
                if self.shared_gen.load(Ordering::SeqCst) != gen {
                    break;
                }
                let next = tokio::select! {
                    biased;
                    next = read.next() => next,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                };
                let Some(next) = next else { break };
                let Ok(Message::Text(text)) = next else { continue };
                let Ok(sm) = serde_json::from_str::<ServerMessage>(&text) else { continue };
                match sm {
                    ServerMessage::LinkJoined { state, .. } => {
                        let ready =
                            state.users.iter().any(|u| u.user_id == host_id && u.is_connected);
                        let users = state.users.iter().filter(|u| u.is_connected).count();
                        let device_ids: Vec<String> = state
                            .users
                            .iter()
                            .filter(|u| u.is_connected)
                            .filter(|u| linked_guest_id(&u.user_id).is_some())
                            .map(|u| u.user_id.clone())
                            .collect();
                        // Seed missing badges from the main room's participant list. A guest that
                        // reconnects keeps the flags already learned from its last hello.
                        let defaults: Vec<SharedControlPeer> = {
                            let inner = self.inner.lock().await;
                            device_ids
                                .iter()
                                .filter_map(|device| linked_guest_id(device))
                                .map(|id| peer_for_user(&inner, &id))
                                .collect()
                        };
                        {
                            let mut inner = self.inner.lock().await;
                            inner.shared_link_ready = ready;
                            inner.shared_link_users = users;
                            inner.shared_link_device_ids = device_ids;
                            let connected: Vec<String> = inner
                                .shared_link_device_ids
                                .iter()
                                .filter_map(|device| linked_guest_id(device))
                                .collect();
                            inner.shared_link_guests.retain(|p| connected.contains(&p.user_id));
                            for peer in defaults {
                                if !inner
                                    .shared_link_guests
                                    .iter()
                                    .any(|p| p.user_id == peer.user_id)
                                {
                                    inner.shared_link_guests.push(peer);
                                }
                            }
                            if ready {
                                inner.shared_link_reason = None;
                            }
                        }
                        let ids = self.inner.lock().await.shared_link_device_ids.clone();
                        self.broadcast_shared_roster(&otx, ids).await;
                        self.emit_state().await;
                    }
                    ServerMessage::UserJoined { user } => {
                        // The link room's join snapshot is authoritative; these keep the roster
                        // live between snapshots.
                        let peer = {
                            let inner = self.inner.lock().await;
                            linked_guest_id(&user.user_id).map(|id| peer_for_user(&inner, &id))
                        };
                        let ids = {
                            let mut inner = self.inner.lock().await;
                            inner.shared_link_users = inner.shared_link_users.saturating_add(1);
                            if peer.is_some()
                                && !inner.shared_link_device_ids.contains(&user.user_id)
                            {
                                inner.shared_link_device_ids.push(user.user_id.clone());
                            }
                            if let Some(peer) = peer {
                                upsert_peer(&mut inner.shared_link_guests, peer);
                            }
                            inner.shared_link_device_ids.clone()
                        };
                        self.broadcast_shared_roster(&otx, ids).await;
                        self.emit_state().await;
                    }
                    ServerMessage::UserReconnected { user_id } => {
                        let peer = {
                            let inner = self.inner.lock().await;
                            linked_guest_id(&user_id).map(|id| peer_for_user(&inner, &id))
                        };
                        let ids = {
                            let mut inner = self.inner.lock().await;
                            inner.shared_link_users = inner.shared_link_users.saturating_add(1);
                            if peer.is_some() && !inner.shared_link_device_ids.contains(&user_id) {
                                inner.shared_link_device_ids.push(user_id.clone());
                            }
                            if let Some(peer) = peer {
                                upsert_peer(&mut inner.shared_link_guests, peer);
                            }
                            inner.shared_link_device_ids.clone()
                        };
                        self.broadcast_shared_roster(&otx, ids).await;
                        self.emit_state().await;
                    }
                    ServerMessage::UserLeft { user_id }
                    | ServerMessage::UserDisconnected { user_id } => {
                        let ids = {
                            let mut inner = self.inner.lock().await;
                            inner.shared_link_users = inner.shared_link_users.saturating_sub(1);
                            inner.shared_link_device_ids.retain(|id| id != &user_id);
                            if let Some(id) = linked_guest_id(&user_id) {
                                inner.shared_link_guests.retain(|p| p.user_id != id);
                            }
                            inner.shared_link_device_ids.clone()
                        };
                        self.broadcast_shared_roster(&otx, ids).await;
                        self.emit_state().await;
                    }
                    ServerMessage::Remote { from, action } => {
                        let Ok(envelope) = serde_json::from_value::<SharedControlEnvelope>(action)
                        else {
                            continue;
                        };
                        match envelope {
                            SharedControlEnvelope::ParticipantHello {
                                username,
                                api_enabled,
                                ui_control,
                                ..
                            } => {
                                let Some(id) = linked_guest_id(&from) else { continue };
                                let ids = {
                                    let mut inner = self.inner.lock().await;
                                    if !inner.shared_link_device_ids.contains(&from) {
                                        inner.shared_link_device_ids.push(from.clone());
                                    }
                                    let mut peer = peer_for_user(&inner, &id);
                                    if !username.trim().is_empty() {
                                        peer.username = username;
                                    }
                                    peer.api_enabled = api_enabled;
                                    peer.ui_control = ui_control;
                                    upsert_peer(&mut inner.shared_link_guests, peer);
                                    inner.shared_link_device_ids.clone()
                                };
                                self.broadcast_shared_roster(&otx, ids).await;
                                self.emit_state().await;
                            }
                            SharedControlEnvelope::ControlRequest {
                                id,
                                action,
                                submitted_by,
                                ..
                            } => {
                                let enabled = {
                                    let inner = self.inner.lock().await;
                                    inner.role == Role::Host && inner.shared_control_enabled
                                };
                                if !enabled {
                                    let _ = otx.send(ClientMessage::Remote {
                                        action: SharedControlEnvelope::ControlResult {
                                            id,
                                            ok: false,
                                            error: Some(
                                                "the host turned shared control off".into(),
                                            ),
                                            flags: None,
                                        }
                                        .to_value(),
                                        to: Some(from),
                                    });
                                    continue;
                                }
                                let _ = self.sync_tx.send(SyncCommand::SharedControlRequest {
                                    id,
                                    from,
                                    action,
                                    submitted_by,
                                });
                            }
                            _ => {}
                        }
                    }
                    ServerMessage::Error { code, message } => {
                        tracing::warn!(%code, %message, "shared-control link error");
                        break;
                    }
                    _ => {}
                }
            }

            if self.shared_gen.load(Ordering::SeqCst) != gen {
                drop(otx);
                if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                    writer.abort();
                }
                return;
            }
            {
                let mut inner = self.inner.lock().await;
                inner.shared_link_ready = false;
                inner.shared_link_users = 0;
                // Guests hold one persistent socket now. If it drops, their slots drop too; a
                // reconnect re-sends hello and restores the badges from live settings.
                inner.shared_link_device_ids.clear();
                inner.shared_link_guests.clear();
            }
            *self.shared_outbound.lock().await = None;
            drop(otx);
            if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                writer.abort();
            }
            self.emit_state().await;
            attempt += 1;
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    async fn set_shared_link_reason(&self, reason: Option<String>) {
        {
            let mut inner = self.inner.lock().await;
            inner.shared_link_reason = reason;
        }
        self.emit_state().await;
    }

    /// Host: send the live roster to every capable guest on the hidden link. `host_allows` comes
    /// from the current opt-in, not from the relay's legacy capability bit.
    async fn broadcast_shared_roster(
        &self,
        otx: &mpsc::UnboundedSender<ClientMessage>,
        device_ids: Vec<String>,
    ) {
        let (host_allows, guests) = {
            let inner = self.inner.lock().await;
            (
                inner.role == Role::Host && inner.shared_control_enabled,
                inner.shared_link_guests.clone(),
            )
        };
        let action = SharedControlEnvelope::ParticipantRoster { host_allows, guests }.to_value();
        for id in device_ids {
            let _ = otx.send(ClientMessage::Remote { action: action.clone(), to: Some(id) });
        }
    }

    /// True when a local Control API mutation may be forwarded to this room's host instead of being
    /// rejected with `listen_together_guest`. The host's opt-in is learned by the hidden link itself;
    /// a base host simply never answers there.
    pub async fn shared_control_allowed(&self) -> bool {
        let inner = self.inner.lock().await;
        inner.role == Role::Guest && inner.room_code.is_some() && inner.status_connected
    }

    /// Why shared control is unavailable in the current room, for the UI and guest API errors.
    /// `None` means there is no room to explain, or everything is ready.
    pub async fn shared_control_reason(&self) -> Option<String> {
        Self::reason_for(&*self.inner.lock().await)
    }

    /// Legacy id list carried in control answers. The rich roster now travels in the hidden-link
    /// handshake, but older fork hosts still get a useful badge list from this field.
    pub async fn capable_guests(&self) -> Vec<String> {
        self.inner.lock().await.shared_link_guests.iter().map(|p| p.user_id.clone()).collect()
    }

    /// Older hosts sent only ids in a control answer. Seed them as unmarked peers until the
    /// hidden-link roster arrives; that keeps mixed-version rooms readable.
    pub async fn note_capable_guests(&self, guests: Vec<String>) {
        {
            let mut inner = self.inner.lock().await;
            if inner.role == Role::Host {
                return;
            }
            let users = inner.users.clone();
            let mut changed = false;
            for id in guests {
                if !inner.shared_link_guests.iter().any(|p| p.user_id == id) {
                    let username = users
                        .iter()
                        .find(|u| u.user_id == id)
                        .map(|u| u.username.clone())
                        .unwrap_or_else(|| id.clone());
                    inner.shared_link_guests.push(SharedControlPeer {
                        user_id: id,
                        username,
                        api_enabled: false,
                        ui_control: false,
                    });
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
        self.emit_state().await;
    }

    /// The single source of truth behind both `sharedControlReason` and the guest error message.
    fn reason_for(inner: &Inner) -> Option<String> {
        if inner.role == Role::None {
            return None;
        }
        if let Some(reason) = &inner.shared_link_reason {
            return Some(reason.clone());
        }
        match inner.role {
            Role::Host => {
                if inner.shared_control_enabled
                    && inner.status_connected
                    && !inner.shared_link_ready
                {
                    Some("shared control is preparing for this room; retry shortly".into())
                } else {
                    None
                }
            }
            Role::Guest => {
                if !inner.status_connected {
                    Some("Listen Together is reconnecting; retry shortly".into())
                } else if !inner.shared_link_host_allows {
                    // Derived from the hidden-link handshake, not from the relay's stale
                    // capability bit: a host that enables the switch mid-room is seen live.
                    Some("the host is not allowing shared control for this room".into())
                } else {
                    None
                }
            }
            Role::None => None,
        }
    }

    /// Guest: keep one persistent hidden-link socket for this room. It opens when either local
    /// capability is enabled, so the host sees the slot on join instead of on the first mutation.
    async fn maybe_start_shared_guest_link(self: &Arc<Self>, code: Option<String>) {
        let Some(code) = code else { return };
        let (eligible, guest, connected) = {
            let inner = self.inner.lock().await;
            (
                inner.control_api_enabled || inner.client_control_enabled,
                inner.role == Role::Guest,
                inner.status_connected,
            )
        };
        if !eligible || !guest || !connected {
            self.stop_shared_guest_link().await;
            return;
        }
        {
            let mut active = self.guest_link_code.lock().await;
            if active.as_deref() == Some(code.as_str()) {
                return;
            }
            *active = Some(code.clone());
        }
        let (secret, host_id) = shared_link_keys(&code);
        let (url, device_id) = {
            let inner = self.inner.lock().await;
            let url = if inner.server_url.is_empty() {
                DEFAULT_SERVER.to_string()
            } else {
                inner.server_url.clone()
            };
            let device_id = inner
                .my_id
                .as_ref()
                .map(|id| format!("sc-guest-{id}"))
                .unwrap_or_else(|| format!("sc-guest-{:016x}", rand::random::<u64>()));
            (url, device_id)
        };
        let gen = self.guest_link_gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.guest_link_outbound.lock().await = None;
        self.guest_link_pending.lock().await.clear();
        {
            let mut inner = self.inner.lock().await;
            inner.shared_link_ready = false;
            inner.shared_link_users = 0;
            inner.shared_link_guests.clear();
            inner.shared_link_local_id = Some(device_id.clone());
            inner.shared_link_host_allows = false;
            inner.shared_link_reason = None;
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.run_shared_guest_link(gen, secret, host_id, device_id, url).await;
        });
        self.emit_state().await;
    }

    async fn stop_shared_guest_link(&self) {
        self.guest_link_gen.fetch_add(1, Ordering::SeqCst);
        *self.guest_link_code.lock().await = None;
        *self.guest_link_outbound.lock().await = None;
        self.guest_link_pending.lock().await.clear();
        {
            let mut inner = self.inner.lock().await;
            inner.shared_link_ready = false;
            inner.shared_link_users = 0;
            inner.shared_link_guests.clear();
            inner.shared_link_local_id = None;
            inner.shared_link_host_allows = false;
            inner.shared_link_reason = None;
        }
        self.emit_state().await;
    }

    /// Guest side of the hidden link: join once, advertise capabilities, keep the socket open and
    /// multiplex correlated control answers onto it. Reconnects with the same device id.
    async fn run_shared_guest_link(
        self: Arc<Self>,
        gen: u64,
        secret: String,
        host_id: String,
        device_id: String,
        url: String,
    ) {
        let mut attempt = 0;
        while self.guest_link_gen.load(Ordering::SeqCst) == gen {
            let ws = match connect_ws(&url).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::warn!(error = %e, "guest shared-control link connect failed");
                    attempt += 1;
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }
            };
            attempt = 0;
            let (mut sink, mut read) = ws.split();
            let (otx, mut orx) = mpsc::unbounded_channel::<ClientMessage>();
            *self.guest_link_outbound.lock().await = Some(otx.clone());
            let _ = otx.send(ClientMessage::JoinLink {
                secret: secret.clone(),
                device_id: device_id.clone(),
                name: "Shared control".into(),
            });
            self.send_guest_hello().await;
            let mut writer = tokio::spawn(async move {
                while let Some(msg) = orx.recv().await {
                    if sink.send(Message::Text(msg.to_json())).await.is_err() {
                        break;
                    }
                }
            });

            loop {
                if self.guest_link_gen.load(Ordering::SeqCst) != gen {
                    break;
                }
                let next = tokio::select! {
                    biased;
                    next = read.next() => next,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                };
                let Some(next) = next else { break };
                let Ok(Message::Text(text)) = next else { continue };
                let Ok(sm) = serde_json::from_str::<ServerMessage>(&text) else { continue };
                match sm {
                    ServerMessage::LinkJoined { state, .. } => {
                        let ready =
                            state.users.iter().any(|u| u.user_id == host_id && u.is_connected);
                        let users = state.users.iter().filter(|u| u.is_connected).count();
                        {
                            let mut inner = self.inner.lock().await;
                            inner.shared_link_ready = ready;
                            inner.shared_link_users = users;
                        }
                        if !ready {
                            self.set_shared_link_reason(Some(
                                "the host is not allowing shared control for this room".into(),
                            ))
                            .await;
                        } else {
                            self.send_guest_hello().await;
                        }
                        self.emit_state().await;
                    }
                    ServerMessage::UserJoined { user } => {
                        if user.user_id == host_id {
                            {
                                let mut inner = self.inner.lock().await;
                                inner.shared_link_ready = true;
                                inner.shared_link_reason = None;
                            }
                            self.send_guest_hello().await;
                            self.emit_state().await;
                        }
                    }
                    ServerMessage::UserReconnected { user_id } => {
                        if user_id == host_id {
                            {
                                let mut inner = self.inner.lock().await;
                                inner.shared_link_ready = true;
                                inner.shared_link_reason = None;
                            }
                            self.send_guest_hello().await;
                            self.emit_state().await;
                        }
                    }
                    ServerMessage::Remote { from, action } => {
                        if from != host_id {
                            continue;
                        }
                        let Ok(envelope) = serde_json::from_value::<SharedControlEnvelope>(action)
                        else {
                            continue;
                        };
                        match envelope {
                            SharedControlEnvelope::ParticipantRoster { host_allows, guests } => {
                                {
                                    let mut inner = self.inner.lock().await;
                                    inner.shared_link_host_allows = host_allows;
                                    inner.shared_link_ready = host_allows;
                                    inner.shared_link_guests = guests;
                                    inner.shared_link_reason = None;
                                }
                                self.emit_state().await;
                            }
                            SharedControlEnvelope::ControlResult { id, ok, error, flags } => {
                                let answer = if ok {
                                    Ok(flags.unwrap_or_default())
                                } else {
                                    Err(error
                                        .unwrap_or_else(|| "the host refused the control".into()))
                                };
                                if let Some(tx) = self.guest_link_pending.lock().await.remove(&id) {
                                    let _ = tx.send(answer);
                                }
                            }
                            _ => {}
                        }
                    }
                    ServerMessage::Error { code, message } => {
                        self.set_shared_link_reason(Some(shared_link_error(&code, &message))).await;
                        break;
                    }
                    _ => {}
                }
            }

            if self.guest_link_gen.load(Ordering::SeqCst) != gen {
                drop(otx);
                if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                    writer.abort();
                }
                return;
            }
            {
                let mut inner = self.inner.lock().await;
                inner.shared_link_ready = false;
                inner.shared_link_users = 0;
                inner.shared_link_host_allows = false;
            }
            *self.guest_link_outbound.lock().await = None;
            drop(otx);
            if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                writer.abort();
            }
            self.emit_state().await;
            attempt += 1;
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    /// Guest: (re)advertise API/UI capability on the open hidden-link socket. The host derives the
    /// user id from the link device id, so the payload only supplies display data and flags.
    async fn send_guest_hello(&self) {
        let (user_id, username, api_enabled, ui_control) = {
            let inner = self.inner.lock().await;
            let username = inner
                .my_id
                .as_deref()
                .and_then(|me| inner.users.iter().find(|u| u.user_id == me))
                .map(|u| u.username.clone())
                .unwrap_or_default();
            (
                inner.my_id.clone().unwrap_or_default(),
                username,
                inner.control_api_enabled,
                inner.client_control_enabled,
            )
        };
        let action =
            SharedControlEnvelope::ParticipantHello { user_id, username, api_enabled, ui_control }
                .to_value();
        if let Some(tx) = self.guest_link_outbound.lock().await.clone() {
            let _ = tx.send(ClientMessage::Remote { action, to: None });
        }
    }

    /// Guest: send a control request on the persistent hidden-link socket and wait for its
    /// correlated answer. Flags are applied by `AppState` after the host replies.
    pub async fn forward_shared_control(
        self: &Arc<Self>,
        action: RemoteAction,
    ) -> Result<SharedControlFlags, String> {
        if !self.shared_control_allowed().await {
            return Err(self.shared_control_reason().await.unwrap_or_else(|| {
                "the host is not allowing shared control for this room".into()
            }));
        }
        let (eligible, host_id, code) = {
            let inner = self.inner.lock().await;
            (
                inner.control_api_enabled || inner.client_control_enabled,
                shared_link_keys(inner.room_code.as_deref().unwrap_or_default()).1,
                inner.room_code.clone(),
            )
        };
        if !eligible {
            return Err("turn on the Control API or player controls to share playback".into());
        }
        self.maybe_start_shared_guest_link(code).await;
        let ready = tokio::time::timeout(SHARED_CONTROL_TIMEOUT, async {
            loop {
                let inner = self.inner.lock().await;
                if inner.shared_link_ready && inner.shared_link_host_allows {
                    return true;
                }
                drop(inner);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if !ready.unwrap_or(false) {
            let reason = self.shared_control_reason().await.unwrap_or_else(|| {
                "the host did not open the shared-control channel in time".into()
            });
            self.set_shared_link_reason(Some(reason.clone())).await;
            return Err(reason);
        }
        // A host that restarted while we were waiting needs the current flags again.
        self.send_guest_hello().await;

        let id = format!(
            "{}-{}",
            self.guest_link_gen.load(Ordering::SeqCst),
            self.control_id.fetch_add(1, Ordering::Relaxed)
        );
        let submitted_by = self.my_username().await;
        let request = ClientMessage::Remote {
            action: SharedControlEnvelope::ControlRequest {
                id: id.clone(),
                action: stamp_shared_action(action, submitted_by.as_deref()),
                shared_control_capable: true,
                submitted_by,
            }
            .to_value(),
            to: Some(host_id),
        };
        let tx = self
            .guest_link_outbound
            .lock()
            .await
            .clone()
            .ok_or_else(|| "the shared-control channel is not open".to_string())?;
        if tx.send(request).is_err() {
            return Err("the shared-control channel closed".into());
        }
        let (answer_tx, answer_rx) = oneshot::channel();
        self.guest_link_pending.lock().await.insert(id.clone(), answer_tx);
        let waited = tokio::time::timeout(SHARED_CONTROL_TIMEOUT, answer_rx).await;
        let result = match waited {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err("the shared-control channel closed".into()),
            Err(_) => Err("the host did not answer within 15 seconds".into()),
        };
        self.guest_link_pending.lock().await.remove(&id);
        match &result {
            Ok(_) => self.set_shared_link_reason(None).await,
            Err(reason) => self.set_shared_link_reason(Some(reason.clone())).await,
        }
        result
    }

    /// Host: answer a guest's request on the hidden link. The fallback keeps older fork relays
    /// working; on the default relay only the hidden link can route to the guest device.
    pub(crate) async fn send_control_result(
        &self,
        id: String,
        to: String,
        result: Result<(), String>,
        flags: Option<SharedControlFlags>,
    ) {
        let msg = ClientMessage::Remote {
            action: SharedControlEnvelope::ControlResult {
                id,
                ok: result.is_ok(),
                error: result.err(),
                flags,
            }
            .to_value(),
            to: Some(to),
        };
        if let Some(tx) = self.shared_outbound.lock().await.clone() {
            let _ = tx.send(msg);
        } else {
            self.send(msg).await;
        }
    }

    /// Our own display name in the room (None outside one) — stamps the host's queue adds.
    pub async fn my_username(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        let me = inner.my_id.as_deref()?;
        inner.users.iter().find(|u| u.user_id == me).map(|u| u.username.clone())
    }

    /// Host: broadcast a playback action to the room. No-op unless we're the host and connected.
    pub async fn broadcast_playback(&self, mut p: Playback) {
        keep_shareable(&mut p);
        let inner = self.inner.lock().await;
        if inner.role != Role::Host {
            return;
        }
        if let Some(tx) = &inner.outbound {
            let _ = tx.send(ClientMessage::Playback(p));
        }
    }

    /// Send any client message on the current connection (best-effort).
    async fn send(&self, msg: ClientMessage) {
        if let Some(tx) = &self.inner.lock().await.outbound {
            let _ = tx.send(msg);
        }
    }

    pub async fn create_room(self: &Arc<Self>, username: String) {
        self.start(ClientMessage::CreateRoom { username }).await;
    }

    pub async fn join_room(self: &Arc<Self>, code: String, username: String) {
        self.start(ClientMessage::JoinRoom { room_code: code, username }).await;
    }

    pub async fn approve_join(&self, user_id: String) {
        self.send(ClientMessage::ApproveJoin { user_id }).await;
    }
    pub async fn reject_join(&self, user_id: String) {
        // Drop it from our local pending list immediately (optimistic).
        {
            let mut inner = self.inner.lock().await;
            inner.pending_joins.retain(|(id, _)| id != &user_id);
        }
        self.send(ClientMessage::RejectJoin { user_id }).await;
        self.emit_state().await;
    }
    pub async fn kick(&self, user_id: String) {
        self.send(ClientMessage::KickUser { user_id }).await;
    }
    pub async fn transfer_host(&self, user_id: String) {
        self.send(ClientMessage::TransferHost { user_id }).await;
    }
    pub async fn suggest(&self, track: Track) {
        if !shareable(&track) {
            return;
        }
        self.send(ClientMessage::Suggest { track }).await;
    }
    /// Host: approve a suggestion. Returns its track so the caller can add it to the real playback
    /// queue (the host owns the queue; the server just notifies the suggester).
    pub async fn approve_suggestion(&self, id: String) -> Option<Track> {
        let track = {
            let mut inner = self.inner.lock().await;
            let idx = inner.suggestions.iter().position(|s| s.id == id);
            idx.map(|i| inner.suggestions.remove(i).track)
        };
        self.send(ClientMessage::ApproveSuggestion { id }).await;
        self.emit_state().await;
        track
    }
    pub async fn reject_suggestion(&self, id: String) {
        {
            let mut inner = self.inner.lock().await;
            inner.suggestions.retain(|s| s.id != id);
        }
        self.send(ClientMessage::RejectSuggestion { id }).await;
        self.emit_state().await;
    }
    pub async fn request_sync(&self) {
        self.send(ClientMessage::RequestSync).await;
    }

    /// Leave the room and tear down the connection.
    pub async fn leave(&self) {
        self.send(ClientMessage::LeaveRoom).await;
        self.gen.fetch_add(1, Ordering::SeqCst); // cancel the connection loop
        self.stop_shared_host_link().await;
        self.stop_shared_guest_link().await;
        self.inner.lock().await.reset_room();
        let _ = self.sync_tx.send(SyncCommand::Release);
        self.emit_state().await;
    }

    /// A snapshot of the client-side LT state for the UI.
    pub async fn snapshot(&self) -> serde_json::Value {
        let inner = self.inner.lock().await;
        Self::snapshot_of(&inner)
    }

    // --- connection loop --------------------------------------------------------------------

    async fn start(self: &Arc<Self>, initial: ClientMessage) {
        // Cancel any existing connection, reset, then spawn a fresh loop.
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.stop_shared_host_link().await;
        self.stop_shared_guest_link().await;
        {
            let mut inner = self.inner.lock().await;
            inner.reset_room();
            inner.connecting = true;
            inner.requesting = true; // waiting for the room to materialize
        }
        self.emit_state().await;
        let gen = self.gen.load(Ordering::SeqCst);
        let me = self.clone();
        tokio::spawn(async move {
            me.run(gen, initial).await;
        });
    }

    async fn run(self: Arc<Self>, gen: u64, initial: ClientMessage) {
        let mut next_msg = initial;
        let mut attempt: u32 = 0;
        loop {
            if self.gen.load(Ordering::SeqCst) != gen {
                return;
            }
            let mut url = self.inner.lock().await.server_url.clone();
            if url.is_empty() {
                url = DEFAULT_SERVER.to_string();
            }
            {
                let mut inner = self.inner.lock().await;
                inner.connecting = true;
                inner.status_connected = false;
            }
            self.emit_state().await;

            match connect_ws(&url).await {
                Ok(ws) => {
                    // A connect that outlived its cancellation must not touch shared state:
                    // `leave()` and a second Host/Join click bump `gen`, but neither can interrupt
                    // an in-flight connect. Without this check a late arrival overwrites the live
                    // connection's `outbound` sender and re-sends its stale CreateRoom/JoinRoom on
                    // a second socket.
                    if self.gen.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    attempt = 0;
                    let (mut sink, mut read) = ws.split();
                    let (otx, mut orx) = mpsc::unbounded_channel::<ClientMessage>();
                    {
                        let mut inner = self.inner.lock().await;
                        inner.outbound = Some(otx.clone());
                        inner.connecting = false;
                        inner.status_connected = true;
                    }
                    let _ = otx.send(next_msg.clone());
                    self.emit_state().await;

                    // Writer: drain outbound → socket.
                    let writer = tokio::spawn(async move {
                        while let Some(m) = orx.recv().await {
                            if sink.send(Message::Text(m.to_json())).await.is_err() {
                                break;
                            }
                        }
                    });
                    // App-level keepalive.
                    let ping = {
                        let tx = otx.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(25)).await;
                                if tx.send(ClientMessage::Ping).is_err() {
                                    break;
                                }
                            }
                        })
                    };

                    let mut last_heard = std::time::Instant::now();
                    loop {
                        // Poll the cancel generation even when idle: `leave()` bumps it but the
                        // server doesn't close the socket on LeaveRoom, so we'd otherwise park here.
                        let next = tokio::select! {
                            biased;
                            next = read.next() => next,
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                                if self.gen.load(Ordering::SeqCst) != gen {
                                    break;
                                }
                                // The server answers every ping, so this much silence is a dead
                                // socket (sleep/resume, a network switch) that would otherwise
                                // sit here, looking connected, until TCP gives up minutes later.
                                if last_heard.elapsed() > SILENCE_LIMIT {
                                    tracing::warn!("listen-together: server went silent, reconnecting");
                                    break;
                                }
                                continue;
                            }
                        };
                        if self.gen.load(Ordering::SeqCst) != gen {
                            break;
                        }
                        let Some(next) = next else { break }; // stream ended
                        last_heard = std::time::Instant::now();
                        match next {
                            Ok(Message::Text(t)) => {
                                match serde_json::from_str::<ServerMessage>(&t) {
                                    Ok(sm) => {
                                        if self.handle(sm).await {
                                            break; // fatal (kicked / rejected) — stop reading
                                        }
                                    }
                                    Err(e) => tracing::debug!(error = %e, "bad server message"),
                                }
                            }
                            Ok(Message::Close(_)) | Err(_) => break,
                            _ => {}
                        }
                    }
                    ping.abort();
                    // Only if we still own the session: a newer connection may have replaced us.
                    if self.gen.load(Ordering::SeqCst) == gen {
                        self.inner.lock().await.outbound = None;
                    }
                    // Let the writer send what is queued and stop once the last sender is gone.
                    // `leave()` queues LeaveRoom right before cancelling us, and aborting could
                    // lose it: the server then took the leave for a dropped socket, kept the room
                    // up and handed the host role on instead of closing it. Bounded, because a
                    // half-open socket can block a write.
                    drop(otx);
                    let mut writer = writer;
                    if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                        writer.abort();
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "listen-together connect failed");
                }
            }

            if self.gen.load(Ordering::SeqCst) != gen {
                return;
            }
            // Reconnect only if we were in a room (have a token). Fresh create/join that never
            // landed just fails.
            let token = self.inner.lock().await.session_token.clone();
            let Some(t) = token else {
                self.close_locally("Couldn't reach the Listen Together server.").await;
                return;
            };
            attempt += 1;
            if attempt > 15 {
                self.close_locally("Lost connection to the room.").await;
                return;
            }
            {
                let mut inner = self.inner.lock().await;
                inner.status_connected = false;
                inner.connecting = true;
            }
            self.emit_state().await;
            next_msg = ClientMessage::Reconnect { session_token: t };
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    /// Handle one server message. Returns `true` if the connection should close (fatal).
    async fn handle(self: &Arc<Self>, mut sm: ServerMessage) -> bool {
        sanitize(&mut sm);
        match sm {
            ServerMessage::Pong => return false,
            // Device-link messages never arrive on a Listen Together socket. Shared control uses
            // its own hidden link socket now, so `Remote` is ignored here too.
            ServerMessage::LinkJoined { .. }
            | ServerMessage::PairCode { .. }
            | ServerMessage::Paired { .. }
            | ServerMessage::LinkStatus { .. }
            | ServerMessage::Remote { .. } => return false,

            ServerMessage::RoomCreated { room_code: _, user_id, session_token, state } => {
                {
                    let mut inner = self.inner.lock().await;
                    inner.role = Role::Host;
                    inner.requesting = false;
                    inner.my_id = Some(user_id);
                    inner.session_token = Some(session_token);
                    inner.absorb(&state);
                }
                // Tell the relay our persisted opt-in before any guest can ask to control us, and
                // open the room-code-derived link room on whichever relay this room uses.
                self.announce_shared_control().await;
                self.maybe_start_shared_host_link().await;
                // Seed the room with whatever we're currently playing.
                let _ = self.sync_tx.send(SyncCommand::HostSeed);
            }

            ServerMessage::JoinRequest { user_id, username } => {
                let is_new = {
                    let mut inner = self.inner.lock().await;
                    let new = !inner.pending_joins.iter().any(|(id, _)| id == &user_id);
                    if new {
                        inner.pending_joins.push((user_id, username.clone()));
                    }
                    new
                };
                if is_new {
                    self.emit_notice(&format!("{username} wants to join")).await;
                }
            }

            ServerMessage::JoinApproved { user_id, session_token, state } => {
                let code = state.room_code.clone();
                {
                    let mut inner = self.inner.lock().await;
                    inner.role = Role::Guest;
                    inner.requesting = false;
                    inner.my_id = Some(user_id);
                    inner.session_token = Some(session_token);
                    inner.absorb(&state);
                }
                // Capable guests open the hidden link at join time, so the host's slot count and
                // badges are live before the first control action.
                self.maybe_start_shared_guest_link(Some(code)).await;
                self.send_guest_hello().await;
                let _ = self.sync_tx.send(SyncCommand::ApplyState(state));
            }

            ServerMessage::JoinRejected { reason } => {
                self.emit_notice(&reason).await;
                self.close_locally("").await;
                return true;
            }

            ServerMessage::Reconnected { user_id, is_host, state } => {
                let code = state.room_code.clone();
                {
                    let mut inner = self.inner.lock().await;
                    inner.role = if is_host { Role::Host } else { Role::Guest };
                    inner.requesting = false;
                    inner.my_id = Some(user_id);
                    inner.absorb(&state);
                }
                if is_host {
                    self.announce_shared_control().await;
                    self.maybe_start_shared_host_link().await;
                } else {
                    self.maybe_start_shared_guest_link(Some(code)).await;
                    self.send_guest_hello().await;
                    let _ = self.sync_tx.send(SyncCommand::ApplyState(state));
                }
            }

            ServerMessage::UserJoined { user } => {
                let mut inner = self.inner.lock().await;
                inner.pending_joins.retain(|(id, _)| id != &user.user_id);
                if !inner.users.iter().any(|u| u.user_id == user.user_id) {
                    inner.users.push(user);
                }
            }
            ServerMessage::UserLeft { user_id } => {
                let mut inner = self.inner.lock().await;
                inner.users.retain(|u| u.user_id != user_id);
                // Also covers a joiner who vanished while still pending: without this the host's
                // request list keeps a row that approving can never clear.
                inner.pending_joins.retain(|(id, _)| id != &user_id);
            }
            ServerMessage::UserDisconnected { user_id } => {
                let mut inner = self.inner.lock().await;
                if let Some(u) = inner.users.iter_mut().find(|u| u.user_id == user_id) {
                    u.is_connected = false;
                }
            }
            ServerMessage::UserReconnected { user_id } => {
                let mut inner = self.inner.lock().await;
                if let Some(u) = inner.users.iter_mut().find(|u| u.user_id == user_id) {
                    u.is_connected = true;
                }
            }

            ServerMessage::HostChanged { host_id } => {
                let became_host = {
                    let mut inner = self.inner.lock().await;
                    for u in &mut inner.users {
                        u.is_host = u.user_id == host_id;
                    }
                    let me = inner.my_id.clone();
                    let became = me.as_deref() == Some(host_id.as_str());
                    inner.role = if became { Role::Host } else { Role::Guest };
                    if !became {
                        // The server re-sends these to the new host; ours can't be acted on now.
                        inner.pending_joins.clear();
                        inner.suggestions.clear();
                    }
                    became
                };
                if became_host {
                    // The relay cleared the old host's opt-in; announce ours and pull the reset
                    // state before any new guest can ask to control us.
                    self.announce_shared_control().await;
                    self.maybe_start_shared_host_link().await;
                    self.request_sync().await;
                    // We now control playback locally — stop applying remote sync.
                    let _ = self.sync_tx.send(SyncCommand::Release);
                    self.emit_notice("You're the host now.").await;
                } else {
                    self.stop_shared_host_link().await;
                    let code = self.inner.lock().await.room_code.clone();
                    self.maybe_start_shared_guest_link(code).await;
                    self.send_guest_hello().await;
                }
            }

            ServerMessage::SyncPlayback(p) => {
                // Only guests apply; the host originated it (server excludes us anyway).
                if self.inner.lock().await.role == Role::Guest {
                    self.apply_playback(p).await;
                }
            }

            ServerMessage::SyncState { state } => {
                {
                    let mut inner = self.inner.lock().await;
                    inner.absorb(&state);
                }
                if self.inner.lock().await.role == Role::Guest {
                    let _ = self.sync_tx.send(SyncCommand::ApplyState(state));
                }
            }

            ServerMessage::SuggestionReceived { suggestion } => {
                // Spotify-Jam-style: guest adds go straight into the queue — no host approval
                // step. Stamp who added it, tell the server it's handled (notifies the guest),
                // and hand the track to the bridge to insert at the guest boundary.
                let mut track = suggestion.track;
                if !shareable(&track) {
                    self.send(ClientMessage::RejectSuggestion { id: suggestion.id }).await;
                    return false;
                }
                track.queued_by = Some(suggestion.from_username.clone());
                // Also set the submitter so the host's enqueue path doesn't overwrite the guest's
                // name with the host's when a bot's requester is absent. The UI dedupes equal names.
                track.submitted_by = Some(suggestion.from_username.clone());
                let title = track.title.clone();
                self.send(ClientMessage::ApproveSuggestion { id: suggestion.id }).await;
                let _ = self.sync_tx.send(SyncCommand::GuestAdd { track });
                self.emit_notice(&format!("{} added {title}", suggestion.from_username)).await;
            }
            ServerMessage::SuggestionApproved { .. } => {
                self.emit_notice("Added to the session queue.").await;
            }
            ServerMessage::SuggestionRejected { reason, .. } => {
                self.emit_notice(&reason).await;
            }

            ServerMessage::Kicked { reason } => {
                self.emit_notice(&reason).await;
                self.close_locally("").await;
                return true;
            }
            ServerMessage::Error { code, message } => {
                self.emit_notice(&message).await;
                // Any error that arrives before we're actually in a room means create/join failed:
                // close instead of leaving the UI spinning on "waiting for the host". Once in a
                // room, only a dead session is fatal (`not_host` etc. are just refusals).
                let in_room = self.inner.lock().await.role != Role::None;
                if !in_room || code == "session_expired" {
                    self.close_locally("").await;
                    return true;
                }
            }
        }
        self.emit_state().await;
        false
    }

    /// Translate a broadcast playback action into a bridge command (guest side).
    async fn apply_playback(&self, p: Playback) {
        // Keep our mirror of current track/queue current so the UI reflects it.
        {
            let mut inner = self.inner.lock().await;
            match p.kind {
                PlaybackKind::ChangeTrack => {
                    inner.current_track = p.track.clone();
                    if let Some(q) = &p.queue {
                        inner.queue = q.clone();
                    }
                }
                PlaybackKind::SyncQueue => {
                    if let Some(q) = &p.queue {
                        inner.queue = q.clone();
                    }
                }
                _ => {}
            }
        }
        let flags = SharedControlFlags {
            shuffle: p.shuffle,
            repeat: p.repeat.clone(),
            capable_guests: None,
        };
        let cmd = match p.kind {
            PlaybackKind::Play => Some(SyncCommand::Play { position_ms: p.position_ms }),
            PlaybackKind::Pause => Some(SyncCommand::Pause { position_ms: p.position_ms }),
            PlaybackKind::Seek => Some(SyncCommand::Seek { position_ms: p.position_ms }),
            PlaybackKind::ChangeTrack => Some(SyncCommand::ChangeTrack {
                track: p.track,
                position_ms: p.position_ms,
                playing: p.playing,
                queue: p.queue.unwrap_or_default(),
            }),
            PlaybackKind::SyncQueue => p.queue.map(|queue| SyncCommand::SyncQueue { queue }),
            PlaybackKind::SetVolume => None,
        };
        if let Some(cmd) = cmd {
            let _ = self.sync_tx.send(cmd);
        }
        if flags.shuffle.is_some() || flags.repeat.is_some() {
            let _ = self
                .sync_tx
                .send(SyncCommand::QueueOptions { shuffle: flags.shuffle, repeat: flags.repeat });
        }
    }

    /// Tear down room membership locally (no server round-trip). `notice` optional.
    async fn close_locally(&self, notice: &str) {
        if !notice.is_empty() {
            self.emit_notice(notice).await;
        }
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.stop_shared_host_link().await;
        self.stop_shared_guest_link().await;
        self.inner.lock().await.reset_room();
        let _ = self.sync_tx.send(SyncCommand::Release);
        self.emit_state().await;
    }

    // --- UI events --------------------------------------------------------------------------

    fn snapshot_of(inner: &Inner) -> serde_json::Value {
        let role = match inner.role {
            Role::None => "none",
            Role::Host => "host",
            Role::Guest => "guest",
        };
        serde_json::json!({
            "status": inner.status().as_str(),
            "role": role,
            "requesting": inner.requesting,
            "roomCode": inner.room_code,
            "myId": inner.my_id,
            "serverUrl": inner.server_url,
            // Sent so the settings panel can show what "the default" actually is once the user
            // opens it. `serverUrl` stays empty until they pick their own, which is what keeps the
            // address off every other screen.
            "defaultServerUrl": DEFAULT_SERVER,
            "users": inner.users,
            "currentTrack": inner.current_track,
            "queue": inner.queue,
            "pendingJoins": inner.pending_joins.iter()
                .map(|(id, name)| serde_json::json!({ "userId": id, "username": name }))
                .collect::<Vec<_>>(),
            "suggestions": inner.suggestions,
            "sharedControlEnabled": inner.shared_control_enabled,
            "sharedControlLinkReady": inner.shared_link_ready,
            // The host is one of the eight link devices; show only the guest slots it can fill.
            "sharedControlLinkGuests": inner.shared_link_device_ids.len(),
            // Room user ids plus live API/UI capabilities, keyed to the participant list.
            "sharedControlGuests": inner.shared_link_guests,
            "sharedControlHostAllows": inner.shared_link_host_allows,
            "sharedControlApiEnabled": inner.control_api_enabled,
            "sharedControlClientControl": inner.client_control_enabled,
            "sharedControlCapable": inner.shared_control_capable,
            "sharedControl": inner.shared_control,
            "hostSharedControlCapable": inner.host_shared_control_capable,
            "sharedControlReason": Self::reason_for(inner),
        })
    }

    async fn emit_state(&self) {
        let snap = { Self::snapshot_of(&*self.inner.lock().await) };
        let _ = self.app.emit("lt-state", snap);
    }

    async fn emit_notice(&self, msg: &str) {
        let _ = self.app.emit("lt-notice", msg);
    }
}

/// Whether a track may cross the wire: only a YouTube video id. Ids come from the relay and from
/// every other member of the room, and the player resolves a `LOCAL:<path>` id straight to a file,
/// so a peer could point us at any path. On Windows a UNC path (`\\host\share`) makes the OS
/// connect to that host and offer the user's NTLM credentials. Outbound, the same rule keeps our
/// local paths, and the username in them, off the relay; guests couldn't play the file anyway.
pub(crate) fn shareable(t: &Track) -> bool {
    t.id.len() == 11 && t.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Stamp the room client onto every track in a forwarded queue action. `queued_by` keeps the
/// bot's requester when there is one; the submitter is carried separately so the host can show
/// both names (guest → twitch user) instead of overwriting one with the other.
pub(crate) fn stamp_shared_action(action: RemoteAction, by: Option<&str>) -> RemoteAction {
    let Some(by) = by.filter(|by| !by.trim().is_empty()) else {
        return action;
    };
    let stamp = |mut tracks: Vec<Track>| {
        for track in &mut tracks {
            if track.submitted_by.is_none() {
                track.submitted_by = Some(by.to_string());
            }
            if track.queued_by.as_deref().unwrap_or("").trim().is_empty() {
                track.queued_by = Some(by.to_string());
            }
        }
        tracks
    };
    match action {
        RemoteAction::PlayNext { tracks, from } => {
            RemoteAction::PlayNext { tracks: stamp(tracks), from }
        }
        RemoteAction::AddToQueue { tracks, from } => {
            RemoteAction::AddToQueue { tracks: stamp(tracks), from }
        }
        other => other,
    }
}

fn keep_shareable(p: &mut Playback) {
    p.track = p.track.take().filter(shareable);
    if let Some(q) = &mut p.queue {
        q.retain(shareable);
    }
}

/// Drop every track that isn't [`shareable`] before anything reads the message. Suggestions are
/// checked where they're handled, so a bad one can be rejected rather than left pending. No
/// wildcard arm: a new message that carries tracks has to be decided on here.
fn sanitize(sm: &mut ServerMessage) {
    match sm {
        ServerMessage::RoomCreated { state, .. }
        | ServerMessage::JoinApproved { state, .. }
        | ServerMessage::Reconnected { state, .. }
        | ServerMessage::SyncState { state }
        | ServerMessage::LinkJoined { state, .. } => {
            state.current_track = state.current_track.take().filter(shareable);
            state.queue.retain(shareable);
        }
        ServerMessage::SyncPlayback(p) => keep_shareable(p),
        ServerMessage::SuggestionReceived { .. }
        | ServerMessage::Pong
        | ServerMessage::JoinRequest { .. }
        | ServerMessage::JoinRejected { .. }
        | ServerMessage::UserJoined { .. }
        | ServerMessage::UserLeft { .. }
        | ServerMessage::UserDisconnected { .. }
        | ServerMessage::UserReconnected { .. }
        | ServerMessage::HostChanged { .. }
        | ServerMessage::SuggestionApproved { .. }
        | ServerMessage::SuggestionRejected { .. }
        | ServerMessage::Kicked { .. }
        | ServerMessage::Error { .. }
        // Link only (`handle` ignores them); a remote `play_track` never reaches this socket.
        | ServerMessage::Remote { .. }
        | ServerMessage::PairCode { .. }
        | ServerMessage::Paired { .. }
        | ServerMessage::LinkStatus { .. } => {}
    }
}

/// How long one address gets to complete a TCP handshake before we move to the next.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the TLS and WebSocket handshakes get once TCP is up. A network that drops the
/// ClientHello used to hang this await for good, and with it the retry loop around it (#732).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// No frame from the server for this long means the connection is dead. Pings go out every 25s
/// and each gets a Pong, so this is two missed answers plus slack.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);

/// Open the WebSocket, giving every resolved address its own short deadline.
///
/// `tokio_tungstenite::connect_async` hands `host:port` straight to `TcpStream::connect`, which
/// walks the resolved addresses one at a time and waits out the OS SYN timeout (~127s on Linux) on
/// each dead one. Tailscale Funnel publishes three AAAA and three A records for a node, and the
/// AAAA ingress is a black hole from some networks; glibc sorts IPv6 first, so a host with working
/// IPv6 burned six minutes on dead addresses before ever trying the IPv4 that works. Same reason a
/// join request never reached the host: the socket carrying it was still in a SYN retry.
pub(crate) async fn connect_ws(
    url: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WsError> {
    let req = url.into_client_request()?;
    let uri = req.uri().clone();
    let host = uri
        .host()
        .ok_or(WsError::Url(tokio_tungstenite::tungstenite::error::UrlError::NoHostName))?;
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("ws") { 80 } else { 443 });

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    let socket = connect_first(&addrs).await.map_err(WsError::Io)?;

    let handshake = tokio_tungstenite::client_async_tls_with_config(req, socket, None, None);
    let (ws, _) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await.map_err(|_| {
        WsError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timed out"))
    })??;
    Ok(ws)
}

/// First address that completes a handshake within [`CONNECT_TIMEOUT`], in order.
async fn connect_first(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last: Option<std::io::Error> = None;
    for &addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => {
                tracing::debug!(%addr, error = %e, "listen-together: address refused");
                last = Some(e);
            }
            Err(_) => {
                tracing::debug!(%addr, "listen-together: address timed out");
                last = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{addr} did not answer in {CONNECT_TIMEOUT:?}"),
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "host resolved to nothing")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            title: String::new(),
            artist: String::new(),
            thumbnail: None,
            duration_ms: 0,
            queued_by: None,
            submitted_by: None,
        }
    }

    #[test]
    fn only_video_ids_cross_the_wire() {
        let mut p = Playback::new(PlaybackKind::ChangeTrack);
        p.track = Some(track(r"LOCAL:\\evil\share\a.mp3"));
        p.queue =
            Some(vec![track("dQw4w9WgXcQ"), track("LOCAL:/home/me/a.mp3"), track("dQw4w9WgXc")]);
        let mut sm = ServerMessage::SyncPlayback(p.clone());
        sanitize(&mut sm);
        let ServerMessage::SyncPlayback(got) = sm else { unreachable!() };
        assert!(got.track.is_none());
        assert_eq!(
            got.queue.unwrap().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["dQw4w9WgXcQ"]
        );

        // Outbound runs the same filter.
        keep_shareable(&mut p);
        assert!(p.track.is_none());
        assert_eq!(p.queue.unwrap().len(), 1);

        assert!(!shareable(&track("../../etc/pas")));
        assert!(shareable(&track("a-B_c123456")));
    }

    #[test]
    fn forwarded_rows_keep_both_the_submitter_and_the_bot_requester() {
        let mut guest = track("dQw4w9WgXcQ");
        guest.queued_by = Some("twitch_viewer".into());
        let action = stamp_shared_action(
            RemoteAction::PlayNext { tracks: vec![guest], from: None },
            Some("room_guest"),
        );
        let RemoteAction::PlayNext { tracks, .. } = action else { unreachable!() };
        assert_eq!(tracks[0].submitted_by.as_deref(), Some("room_guest"));
        assert_eq!(tracks[0].queued_by.as_deref(), Some("twitch_viewer"));

        // No requester from the bot: the room client is the only name, once.
        let action = stamp_shared_action(
            RemoteAction::AddToQueue { tracks: vec![track("a-B_c123456")], from: None },
            Some("room_guest"),
        );
        let RemoteAction::AddToQueue { tracks, .. } = action else { unreachable!() };
        assert_eq!(tracks[0].submitted_by.as_deref(), Some("room_guest"));
        assert_eq!(tracks[0].queued_by.as_deref(), Some("room_guest"));
    }

    /// The bug: a dead address ahead of a live one used to cost a full OS SYN timeout (~127s on
    /// Linux, six minutes across Funnel's three AAAA records). It must now fall through fast.
    #[tokio::test]
    async fn dead_address_does_not_block_the_live_one() {
        let live = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live.local_addr().unwrap();
        // 198.51.100.0/24 is TEST-NET-2: routed nowhere, so this either hangs (→ our timeout) or
        // errors out, which is exactly the fall-through we want to exercise.
        let dead: SocketAddr = "198.51.100.1:443".parse().unwrap();

        let t = std::time::Instant::now();
        let s = connect_first(&[dead, live_addr]).await.expect("should reach the live address");
        assert_eq!(s.peer_addr().unwrap(), live_addr);
        assert!(
            t.elapsed() < CONNECT_TIMEOUT * 2,
            "fell through in {:?}, expected under {:?}",
            t.elapsed(),
            CONNECT_TIMEOUT * 2
        );
    }

    /// The bug (#732): a server that takes the TCP connection and never answers the TLS hello hung
    /// `connect_ws` for good, and the retry loop around it. It must fail instead.
    #[tokio::test]
    async fn a_silent_handshake_fails_instead_of_hanging() {
        // The kernel completes the TCP handshake from the backlog; nothing ever answers after it.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("wss://127.0.0.1:{}/ws", silent.local_addr().unwrap().port());
        let r = tokio::time::timeout(HANDSHAKE_TIMEOUT * 2, connect_ws(&url)).await;
        assert!(matches!(r, Ok(Err(_))), "the handshake should time out, not hang");
    }

    #[test]
    fn the_old_default_server_means_the_default() {
        assert_eq!(stored_url(OLD_DEFAULT_SERVER.into()), "");
        assert_eq!(stored_url("wss://my.host/ws".into()), "wss://my.host/ws");
    }

    #[test]
    fn room_code_seeds_a_valid_hidden_link_secret() {
        let (secret, host_id) = shared_link_keys("ABCD1234");
        let (again, same_host) = shared_link_keys("ABCD1234");
        assert_eq!(secret, again);
        assert_eq!(host_id, same_host);
        assert_eq!(secret.len(), 43);
        assert!(secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert!(host_id.starts_with("sc-host-"));
        assert_ne!(secret, shared_link_keys("ABCD1235").0);
    }

    #[test]
    fn shared_control_reason_comes_from_the_handshake_not_the_relay_bit() {
        let mut inner = Inner::default();
        inner.role = Role::Guest;
        assert_eq!(
            LtSession::reason_for(&inner).as_deref(),
            Some("Listen Together is reconnecting; retry shortly")
        );

        inner.status_connected = true;
        assert_eq!(
            LtSession::reason_for(&inner).as_deref(),
            Some("the host is not allowing shared control for this room")
        );

        // A stale relay capability bit must never claim the host is allowing control.
        inner.host_shared_control_capable = true;
        inner.shared_control = true;
        assert_eq!(
            LtSession::reason_for(&inner).as_deref(),
            Some("the host is not allowing shared control for this room")
        );

        inner.shared_link_host_allows = true;
        assert_eq!(LtSession::reason_for(&inner), None);

        inner.shared_link_reason =
            Some("the host is not allowing shared control for this room".into());
        assert_eq!(
            LtSession::reason_for(&inner).as_deref(),
            Some("the host is not allowing shared control for this room")
        );
    }

    #[test]
    fn shared_control_reason_shows_the_host_link_window() {
        let mut inner = Inner::default();
        inner.role = Role::Host;
        inner.shared_control_enabled = true;
        inner.status_connected = true;
        assert_eq!(
            LtSession::reason_for(&inner).as_deref(),
            Some("shared control is preparing for this room; retry shortly")
        );

        inner.shared_link_ready = true;
        assert_eq!(LtSession::reason_for(&inner), None);

        inner.shared_control_enabled = false;
        inner.shared_link_ready = false;
        assert_eq!(LtSession::reason_for(&inner), None);
    }

    #[test]
    fn snapshot_counts_and_badges_live_hidden_link_guests() {
        let mut inner = Inner::default();
        inner.role = Role::Host;
        inner.shared_link_ready = true;
        inner.shared_link_users = 4; // host + three guests
        inner.shared_link_device_ids =
            vec!["sc-guest-g1".into(), "sc-guest-g2".into(), "sc-guest-g3".into()];
        inner.shared_link_guests = vec![
            SharedControlPeer {
                user_id: "g1".into(),
                username: "Ann".into(),
                api_enabled: true,
                ui_control: true,
            },
            SharedControlPeer {
                user_id: "g2".into(),
                username: "Ben".into(),
                api_enabled: true,
                ui_control: false,
            },
            SharedControlPeer {
                user_id: "g3".into(),
                username: "Cy".into(),
                api_enabled: false,
                ui_control: true,
            },
        ];
        let snap = LtSession::snapshot_of(&inner);
        assert_eq!(snap["sharedControlLinkReady"], true);
        assert_eq!(snap["sharedControlLinkGuests"], 3);
        assert_eq!(snap["sharedControlGuests"][0]["userId"], "g1");
        assert_eq!(snap["sharedControlGuests"][0]["apiEnabled"], true);
        assert_eq!(snap["sharedControlGuests"][0]["uiControl"], true);
        assert_eq!(snap["sharedControlGuests"][1]["uiControl"], false);
        assert_eq!(snap["sharedControlGuests"][2]["apiEnabled"], false);

        inner.shared_link_device_ids.clear();
        inner.shared_link_guests.clear();
        let snap = LtSession::snapshot_of(&inner);
        assert_eq!(snap["sharedControlLinkGuests"], 0);
        assert_eq!(snap["sharedControlGuests"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn hidden_link_device_ids_name_the_room_participant() {
        assert_eq!(
            linked_guest_id("sc-guest-user_123_456-9a8b7c6d").as_deref(),
            Some("user_123_456")
        );
        assert_eq!(linked_guest_id("sc-host-abc123"), None);
        assert_eq!(linked_guest_id("sc-guest-"), None);
    }

    #[tokio::test]
    async fn no_addresses_is_an_error_not_a_hang() {
        assert!(connect_first(&[]).await.is_err());
    }
}

/// Exponential backoff, capped at 32s (context/19 Part B §1: the client's real ceiling).
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.clamp(1, 6) - 1; // 0..=5 → 1,2,4,8,16,32
    Duration::from_secs(1u64 << shift)
}
