//! Optional OBS Tuna output.
//!
//! Tuna's browser source POSTs one JSON object to a local HTTP port. We mirror that shape from the
//! app's authoritative playback snapshot, not from hub ticks: the hub only tells us *when* to push
//! (track change, transport change, or the worker's ~1 Hz timer).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};

use super::hub::HubEvent;
use super::settings::{TunaSettings, TunaStatus};
use crate::control_api::types::{ApiTrack, ControlApiContext};

const PUSH_TIMEOUT: Duration = Duration::from_secs(3);
const TICK_INTERVAL: Duration = Duration::from_secs(1);
const DUPLICATE_WINDOW: Duration = Duration::from_millis(250);
const MAX_BACKOFF_SECS: u64 = 30;

/// The facts needed for one payload. Every field comes from the effective app snapshot.
#[derive(Clone, Debug, Default)]
pub struct TunaSnapshot {
    pub track: Option<ApiTrack>,
    pub paused: bool,
    pub position_sec: f64,
    pub duration_sec: f64,
}

#[async_trait]
pub trait TunaSource: Send + Sync + 'static {
    fn hub(&self) -> broadcast::Sender<HubEvent>;
    async fn snapshot(&self) -> TunaSnapshot;
}

/// Production bridge: the control API context already projects the live queue row as `ApiTrack`,
/// which is the only place album_id and artist_list survive the slim `now-playing` event.
struct AppTunaSource {
    context: Arc<dyn ControlApiContext>,
}

#[async_trait]
impl TunaSource for AppTunaSource {
    fn hub(&self) -> broadcast::Sender<HubEvent> {
        self.context.ui_events()
    }

    async fn snapshot(&self) -> TunaSnapshot {
        let snap = self.context.playback_snapshot().await;
        TunaSnapshot {
            track: self.context.current_api_track().await,
            paused: snap.get("paused").and_then(serde_json::Value::as_bool).unwrap_or(true),
            position_sec: snap.get("position").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
            duration_sec: snap.get("duration").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TunaData {
    cover_url: Option<String>,
    title: String,
    artists: Vec<String>,
    status: &'static str,
    progress: i64,
    duration: i64,
    album_url: Option<String>,
    album: Option<String>,
}

#[derive(Serialize)]
struct TunaRequest<'a> {
    data: TunaData,
    hostname: &'a str,
    date: i64,
}

#[derive(Default)]
struct StatusFields {
    last_ok_at_ms: Option<i64>,
    last_error: Option<String>,
    backoff_secs: Option<u64>,
    retry_at: Option<Instant>,
    last_signature: Option<(String, Instant)>,
}

struct RunningTuna {
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// Runtime state shared by the command surface and the single worker task. No payload is ever
/// logged; failures are only recorded in `last_error` for the Settings status line.
struct TunaRuntime {
    settings: RwLock<TunaSettings>,
    status: Mutex<StatusFields>,
    running: Mutex<Option<RunningTuna>>,
    force: RwLock<Option<mpsc::UnboundedSender<oneshot::Sender<()>>>>,
}

/// Owns the optional worker. `apply` starts, stops or restarts it with a new URL in place.
#[derive(Clone)]
pub struct TunaOutput {
    source: Arc<dyn TunaSource>,
    inner: Arc<TunaRuntime>,
}

impl TunaOutput {
    pub fn new(source: Arc<dyn TunaSource>) -> Self {
        let inner = Arc::new(TunaRuntime {
            settings: RwLock::new(TunaSettings::default()),
            status: Mutex::new(StatusFields::default()),
            running: Mutex::new(None),
            force: RwLock::new(None),
        });
        Self { source, inner }
    }

    /// Production constructor: read snapshots through the same context the control API uses.
    pub fn for_context(context: Arc<dyn ControlApiContext>) -> Self {
        Self::new(Arc::new(AppTunaSource { context }))
    }

    pub async fn apply(&self, settings: TunaSettings) {
        self.stop().await;
        *self.inner.settings.write().await = settings.clone();
        *self.inner.status.lock().await = StatusFields::default();
        if !settings.enabled {
            *self.inner.force.write().await = None;
            return;
        }
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let (force, force_rx) = mpsc::unbounded_channel();
        // Replace the command-side sender so a restarted worker cannot be left unreachable.
        *self.inner.force.write().await = Some(force);
        let source = self.source.clone();
        let inner = self.inner.clone();
        let url = settings.url.clone();
        let task = tokio::spawn(async move {
            Self::run(source, inner, url, force_rx, &mut shutdown_rx).await;
        });
        *self.inner.running.lock().await = Some(RunningTuna { shutdown, task });
    }

    pub async fn stop(&self) {
        *self.inner.force.write().await = None;
        let running = self.inner.running.lock().await.take();
        if let Some(running) = running {
            let _ = running.shutdown.send(());
            running.task.abort();
            let _ = running.task.await;
        }
    }

    pub async fn status(&self) -> TunaStatus {
        let settings = self.inner.settings.read().await.clone();
        let status = self.inner.status.lock().await;
        TunaStatus {
            enabled: settings.enabled,
            url: settings.url,
            last_ok_at_ms: status.last_ok_at_ms,
            last_error: status.last_error.clone(),
            backoff_secs: status.backoff_secs,
        }
    }

    /// `tuna_test_push`: make one attempt now and report the resulting status.
    pub async fn test_push(&self) -> TunaStatus {
        let (ack, rx) = oneshot::channel();
        let sender = self.inner.force.read().await.clone();
        let sent = sender.as_ref().is_some_and(|sender| sender.send(ack).is_ok());
        if !sent {
            let mut status = self.inner.status.lock().await;
            status.last_error = Some("Tuna output is disabled".to_string());
            drop(status);
            return self.status().await;
        }
        let _ = tokio::time::timeout(PUSH_TIMEOUT, rx).await;
        self.status().await
    }

    async fn run(
        source: Arc<dyn TunaSource>,
        inner: Arc<TunaRuntime>,
        url: String,
        mut force_rx: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
        shutdown: &mut oneshot::Receiver<()>,
    ) {
        let mut hub_rx = source.hub().subscribe();
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let hostname = hostname();
        loop {
            tokio::select! {
                _ = &mut *shutdown => break,
                Some(ack) = force_rx.recv() => {
                    let snapshot = source.snapshot().await;
                    Self::push(&inner, &url, &hostname, snapshot).await;
                    let _ = ack.send(());
                }
                event = hub_rx.recv() => {
                    match event {
                        Ok(HubEvent::NowPlaying(_))
                        | Ok(HubEvent::PlaybackState(_))
                        | Err(broadcast::error::RecvError::Lagged(_)) => {
                            let snapshot = source.snapshot().await;
                            Self::push(&inner, &url, &hostname, snapshot).await;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tick.tick() => {
                    let snapshot = source.snapshot().await;
                    if !snapshot.paused && snapshot.track.is_some() {
                        Self::push(&inner, &url, &hostname, snapshot).await;
                    }
                }
            }
        }
    }

    async fn push(inner: &TunaRuntime, url: &str, hostname: &str, snapshot: TunaSnapshot) {
        let now = Instant::now();
        {
            let status = inner.status.lock().await;
            if status.retry_at.is_some_and(|at| now < at) {
                return;
            }
        }
        let payload = build_payload(&snapshot, hostname);
        // `date` changes on every build, so compare the rest of the envelope. The hub emits both
        // now-playing and playback-state for one press; their snapshots are identical and only the
        // first push should cross the wire. Real changes in status, track or progress get a new
        // signature and go out immediately.
        let signature =
            format!("{hostname}|{}", serde_json::to_string(&payload.data).unwrap_or_default());
        {
            let mut status = inner.status.lock().await;
            if let Some((last, at)) = &status.last_signature {
                if *last == signature && now.saturating_duration_since(*at) < DUPLICATE_WINDOW {
                    return;
                }
            }
            status.last_signature = Some((signature, now));
        }
        let result = crate::http::client()
            .post(url)
            .timeout(PUSH_TIMEOUT)
            .json(&payload)
            .send()
            .await
            .and_then(|response| response.error_for_status());
        let mut status = inner.status.lock().await;
        match result {
            Ok(_) => {
                status.last_ok_at_ms = Some(now_ms());
                status.last_error = None;
                status.backoff_secs = None;
                status.retry_at = None;
            }
            Err(error) => {
                let secs = status
                    .backoff_secs
                    .map_or(1, |previous| previous.saturating_mul(2).min(MAX_BACKOFF_SECS));
                status.backoff_secs = Some(secs);
                status.retry_at = Some(now + Duration::from_secs(secs));
                status.last_error = Some(error.to_string());
            }
        }
    }
}

/// Tuna's own browser script only emits `playing` and `stopped`. A paused track is reported as
/// `stopped` so Tuna stops treating it as live audio and shows its placeholder text; we still send
/// that one state change immediately, then no periodic pushes until playback resumes.
fn build_payload<'a>(snapshot: &TunaSnapshot, hostname: &'a str) -> TunaRequest<'a> {
    let (status, track) = match &snapshot.track {
        None => ("stopped", None),
        Some(track) if snapshot.paused => ("stopped", Some(track)),
        Some(track) => ("playing", Some(track)),
    };
    let (title, artists, cover_url, album, album_url) = match track {
        None => (String::new(), Vec::new(), None, None, None),
        Some(track) => {
            let artists = if track.artist_list.is_empty() {
                track.artists.clone().into_iter().collect()
            } else {
                track.artist_list.clone()
            };
            let album_url =
                track.album_id.as_ref().map(|id| format!("https://music.youtube.com/browse/{id}"));
            (track.title.clone(), artists, track.thumbnail.clone(), track.album.clone(), album_url)
        }
    };
    TunaRequest {
        data: TunaData {
            cover_url,
            title,
            artists,
            status,
            progress: seconds_to_ms(snapshot.position_sec),
            duration: seconds_to_ms(snapshot.duration_sec),
            album_url,
            album,
        },
        hostname,
        date: now_ms(),
    }
}

fn seconds_to_ms(seconds: f64) -> i64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * 1000.0) as i64
    } else {
        0
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Existing dependency in this crate; no new hostname crate is needed. A trimmed empty result is
/// replaced so Tuna never receives an empty identity.
fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().trim().to_string();
    if name.is_empty() {
        "limusic".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream};
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Mutex as StdMutex;

    use serde_json::{json, Value};

    use super::*;
    use crate::control_api::types::ApiTrack;

    /// A tiny local stand-in for Tuna's browser-source port. It records the JSON bodies and can be
    /// told to fail, which is enough to verify the payload, backoff and live-apply behavior without
    /// touching the network or the real app.
    struct Capture {
        addr: SocketAddr,
        posts: Arc<StdMutex<Vec<Value>>>,
        status: Arc<AtomicU16>,
    }

    impl Capture {
        fn new() -> Self {
            let listener = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let posts = Arc::new(StdMutex::new(Vec::new()));
            let status = Arc::new(AtomicU16::new(200));
            let (posts_thread, status_thread) = (posts.clone(), status.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    if let Ok(value) = read_request(&mut stream) {
                        posts_thread.lock().unwrap().push(value);
                    }
                    let code = status_thread.load(Ordering::SeqCst);
                    let reason = if code == 200 { "OK" } else { "Simulated failure" };
                    let response = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self { addr, posts, status }
        }

        fn url(&self) -> String {
            format!("http://{}/", self.addr)
        }

        fn set_status(&self, code: u16) {
            self.status.store(code, Ordering::SeqCst);
        }

        fn posts(&self) -> Vec<Value> {
            self.posts.lock().unwrap().clone()
        }

        fn clear(&self) {
            self.posts.lock().unwrap().clear();
        }

        async fn wait(&self, n: usize) -> Vec<Value> {
            for _ in 0..50 {
                let posts = self.posts();
                if posts.len() >= n {
                    return posts;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.posts()
        }
    }

    fn read_request(stream: &mut TcpStream) -> std::io::Result<Value> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut header_end = None;
        loop {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(std::io::Error::other("connection closed before body"));
            }
            buf.extend_from_slice(&tmp[..n]);
            if header_end.is_none() {
                header_end = find(&buf, b"\r\n\r\n").map(|at| at + 4);
            }
            if let Some(end) = header_end {
                let len = content_length(&buf[..end]).unwrap_or(0);
                if buf.len() >= end + len {
                    return serde_json::from_slice(&buf[end..end + len])
                        .map_err(|e| std::io::Error::other(e.to_string()));
                }
            }
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    fn content_length(header: &[u8]) -> Option<usize> {
        let header = String::from_utf8_lossy(header);
        header.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
    }

    #[derive(Clone)]
    struct TestSource {
        hub: broadcast::Sender<HubEvent>,
        snapshot: Arc<StdMutex<TunaSnapshot>>,
    }

    #[async_trait]
    impl TunaSource for TestSource {
        fn hub(&self) -> broadcast::Sender<HubEvent> {
            self.hub.clone()
        }

        async fn snapshot(&self) -> TunaSnapshot {
            self.snapshot.lock().unwrap().clone()
        }
    }

    fn source(snapshot: TunaSnapshot) -> (Arc<TestSource>, broadcast::Sender<HubEvent>) {
        let (hub, _) = broadcast::channel(16);
        let source =
            Arc::new(TestSource { hub: hub.clone(), snapshot: Arc::new(StdMutex::new(snapshot)) });
        (source, hub)
    }

    fn playing() -> TunaSnapshot {
        TunaSnapshot {
            track: Some(ApiTrack {
                video_id: "v123".into(),
                title: "Song".into(),
                artists: Some("Artist".into()),
                artist_list: vec!["Artist".into()],
                album: Some("Album".into()),
                album_id: Some("album123".into()),
                duration: Some("3:00".into()),
                duration_sec: Some(180),
                thumbnail: Some("https://img.example/cover.jpg".into()),
                explicit: false,
                is_video: false,
                live: false,
                queued_by: Some("friend".into()),
                queued: true,
                queued_end: false,
            }),
            paused: true, // start paused so the worker's first tick cannot race the test push
            position_sec: 12.345,
            duration_sec: 180.5,
        }
    }

    #[tokio::test]
    async fn smoke_posts_the_exact_tuna_shape_with_ms_integers() {
        let capture = Capture::new();
        let (src, _) = source(playing());
        let output = TunaOutput::new(src.clone());
        output.apply(TunaSettings { enabled: true, url: capture.url() }).await;

        // A paused ticker sends nothing; only the explicit push crosses the wire.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(capture.posts().is_empty());

        src.snapshot.lock().unwrap().paused = false;
        let status = output.test_push().await;
        let posts = capture.wait(1).await;
        assert_eq!(posts.len(), 1);
        let payload = &posts[0];
        let outer: HashSet<&str> =
            payload.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(outer, HashSet::from(["data", "hostname", "date"]));
        assert_eq!(payload["hostname"].as_str().unwrap_or_default().is_empty(), false);
        assert!(payload["date"].is_i64());
        assert!(payload["date"].as_i64().unwrap() > 1_000_000_000_000);
        let data = &payload["data"];
        let keys: HashSet<&str> = data.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            HashSet::from([
                "cover_url",
                "title",
                "artists",
                "status",
                "progress",
                "duration",
                "album_url",
                "album",
            ])
        );
        assert_eq!(data["status"], "playing");
        assert_eq!(data["title"], "Song");
        assert_eq!(data["artists"], json!(["Artist"]));
        assert_eq!(data["cover_url"], "https://img.example/cover.jpg");
        assert_eq!(data["album"], "Album");
        assert_eq!(data["album_url"], "https://music.youtube.com/browse/album123");
        assert_eq!(data["progress"], json!(12_345));
        assert_eq!(data["duration"], json!(180_500));
        assert!(data["progress"].is_i64());
        assert!(data["duration"].is_i64());
        assert!(status.last_ok_at_ms.is_some());
        assert_eq!(status.backoff_secs, None);

        src.snapshot.lock().unwrap().paused = true;
        output.test_push().await;
        let posts = capture.wait(2).await;
        assert_eq!(posts[1]["data"]["status"], "stopped");
        assert_eq!(posts[1]["data"]["cover_url"], "https://img.example/cover.jpg");

        *src.snapshot.lock().unwrap() = TunaSnapshot::default();
        output.test_push().await;
        let posts = capture.wait(3).await;
        assert_eq!(posts[2]["data"]["status"], "stopped");
        assert_eq!(posts[2]["data"]["title"], "");
        output.stop().await;
    }

    #[tokio::test]
    async fn pushes_on_track_and_state_changes_and_ticks_only_while_playing() {
        let capture = Capture::new();
        let (src, hub) = source(playing());
        let output = TunaOutput::new(src.clone());
        output.apply(TunaSettings { enabled: true, url: capture.url() }).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(capture.posts().is_empty());

        // Start live so the next event is a real playing -> stopped transition, not a duplicate.
        src.snapshot.lock().unwrap().paused = false;
        hub.send(HubEvent::NowPlaying(json!({}))).unwrap();
        assert_eq!(capture.wait(1).await.len(), 1);
        // The paused/stopped state change is pushed immediately, using Tuna's only two live states.
        src.snapshot.lock().unwrap().paused = true;
        hub.send(HubEvent::PlaybackState("paused".into())).unwrap();
        let posts = capture.wait(2).await;
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1]["data"]["status"], "stopped");

        // A paused source gets no periodic posts, even though the worker keeps ticking.
        capture.clear();
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(capture.posts().is_empty());

        src.snapshot.lock().unwrap().paused = false;
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let posts = capture.posts();
        assert!((1..=4).contains(&posts.len()), "expected ~1 Hz ticks, got {}", posts.len());
        output.stop().await;
    }

    #[tokio::test]
    async fn coalesces_identical_immediate_pushes_but_not_changed_content() {
        let capture = Capture::new();
        let (src, hub) = source(playing()); // paused: no ticker in this test
        let output = TunaOutput::new(src.clone());
        output.apply(TunaSettings { enabled: true, url: capture.url() }).await;
        tokio::time::sleep(Duration::from_millis(20)).await; // let the worker subscribe

        // One play press emits both events with the same snapshot a few milliseconds apart.
        hub.send(HubEvent::NowPlaying(json!({}))).unwrap();
        hub.send(HubEvent::PlaybackState("playing".into())).unwrap();
        assert_eq!(capture.wait(1).await.len(), 1);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(capture.posts().len(), 1, "identical immediate push was not coalesced");

        // Progress is real content: it must not be swallowed by the 250 ms window.
        src.snapshot.lock().unwrap().position_sec += 5.0;
        hub.send(HubEvent::NowPlaying(json!({}))).unwrap();
        assert_eq!(capture.wait(2).await.len(), 2);

        // After the window the same payload is a fresh push again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        hub.send(HubEvent::NowPlaying(json!({}))).unwrap();
        assert_eq!(capture.wait(3).await.len(), 3);
        output.stop().await;
    }

    #[tokio::test]
    async fn backs_off_after_failure_and_resets_after_success() {
        let capture = Capture::new();
        capture.set_status(500);
        let (src, _) = source(playing());
        let output = TunaOutput::new(src);
        output.apply(TunaSettings { enabled: true, url: capture.url() }).await;

        let status = output.test_push().await;
        assert_eq!(status.backoff_secs, Some(1));
        assert!(status.last_error.is_some());
        assert!(status.last_ok_at_ms.is_none());
        assert_eq!(capture.posts().len(), 1);

        capture.set_status(200);
        output.test_push().await; // inside the backoff window: no attempt
        assert_eq!(capture.posts().len(), 1);

        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let status = output.test_push().await;
        assert_eq!(capture.wait(2).await.len(), 2);
        assert_eq!(status.backoff_secs, None);
        assert!(status.last_error.is_none());
        assert!(status.last_ok_at_ms.is_some());
        output.stop().await;
    }

    #[tokio::test]
    async fn settings_can_start_stop_and_restart_the_worker_with_a_new_url() {
        let (src, _) = source(playing());
        let output = TunaOutput::new(src);
        let (first, second) = (Capture::new(), Capture::new());

        output.apply(TunaSettings { enabled: false, url: first.url() }).await;
        let status = output.test_push().await;
        assert!(!status.enabled);
        assert!(status.last_error.is_some());
        assert!(first.posts().is_empty());

        output.apply(TunaSettings { enabled: true, url: first.url() }).await;
        output.test_push().await;
        assert_eq!(first.wait(1).await.len(), 1);

        output.apply(TunaSettings { enabled: true, url: second.url() }).await;
        let status = output.test_push().await;
        assert_eq!(status.url, second.url());
        assert_eq!(second.wait(1).await.len(), 1);
        assert_eq!(first.posts().len(), 1);

        output.apply(TunaSettings { enabled: false, url: second.url() }).await;
        output.test_push().await;
        assert_eq!(first.posts().len(), 1);
        assert_eq!(second.posts().len(), 1);
        output.stop().await;
    }
}
