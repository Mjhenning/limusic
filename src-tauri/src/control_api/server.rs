//! Lifecycle for the local control API server.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::serve;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, RwLock};

use crate::db::Db;
use crate::integrations::settings::{
    ControlApiSettings, ControlApiStatus, TunaSettings, TunaStatus, CONTROL_API_SETTINGS_KEY,
    CONTROL_API_TOKEN_KEY, TUNA_SETTINGS_KEY,
};
use crate::integrations::token::{generate_token, is_well_formed};
use crate::integrations::tuna::TunaOutput;
use crate::state::AppState;

use super::handlers::router;
use super::types::{ApiShared, ControlApiContext};
use super::ws::WsConnections;

struct RunningServer {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct Inner {
    server: Option<RunningServer>,
    status: ControlApiStatus,
}

/// Tauri-managed owner of the control API. Stored as `Arc<Self>` because both commands and the
/// app-exit handler need it. Token and status are shared separately so requests read the current
/// token without waiting for the lifecycle lock.
#[derive(Clone)]
pub struct IntegrationsManager {
    db: Arc<Db>,
    context: Arc<dyn ControlApiContext>,
    inner: Arc<Mutex<Inner>>,
    token: Arc<RwLock<String>>,
    mutation: Arc<Mutex<()>>,
    ws: Arc<WsConnections>,
    tuna: TunaOutput,
}

impl IntegrationsManager {
    pub fn new(state: Arc<AppState>) -> Self {
        let token = load_token(&state.db);
        let context: Arc<dyn ControlApiContext> = state.clone();
        let tuna = TunaOutput::for_context(context.clone());
        Self {
            db: state.db.clone(),
            context,
            inner: Arc::new(Mutex::new(Inner::default())),
            token: Arc::new(RwLock::new(token)),
            mutation: Arc::new(Mutex::new(())),
            ws: Arc::new(WsConnections::default()),
            tuna,
        }
    }

    fn shared(&self) -> Arc<ApiShared> {
        Arc::new(ApiShared {
            token: self.token.clone(),
            context: self.context.clone(),
            mutation: self.mutation.clone(),
            hub: self.context.ui_events(),
            ws: self.ws.clone(),
        })
    }

    /// Called from Tauri setup. A bad saved setting is reported through `last_error`, never by
    /// crashing startup.
    pub async fn start_from_settings(&self) {
        self.apply(load_settings(&self.db), "startup").await;
        self.tuna.apply(load_tuna_settings(&self.db)).await;
    }

    /// Apply already-validated settings from the settings command.
    pub async fn apply_settings(&self, settings: ControlApiSettings, reason: &'static str) {
        self.apply(settings, reason).await;
    }

    /// Apply Tuna settings live: start, stop, or restart with a new URL.
    pub async fn apply_tuna_settings(&self, settings: TunaSettings) {
        self.tuna.apply(settings).await;
    }

    pub async fn tuna_status(&self) -> TunaStatus {
        self.tuna.status().await
    }

    pub async fn tuna_test_push(&self) -> TunaStatus {
        self.tuna.test_push().await
    }

    /// Replace the token in shared state, persist it, and restart so every WS connection is
    /// dropped. The new token is never logged.
    pub async fn regenerate_token(&self) -> Result<String, String> {
        let token = generate_token();
        self.db.set_setting(CONTROL_API_TOKEN_KEY, &token);
        *self.token.write().await = token.clone();
        self.apply(load_settings(&self.db), "token regenerated").await;
        Ok(token)
    }

    pub async fn control_api_status(&self) -> ControlApiStatus {
        let settings = load_settings(&self.db);
        let inner = self.inner.lock().await;
        let mut status = inner.status.clone();
        status.ws_clients = self.ws.count();
        status.enabled = settings.enabled;
        if !status.running {
            status.bind_addr = settings.enabled.then(|| settings.bind.clone());
            status.port = settings.enabled.then_some(settings.port);
        }
        status
    }

    /// Stop every optional output. Called on app exit only; a control-API rebind must not stop Tuna.
    pub async fn stop(&self) {
        self.stop_control_api().await;
        self.tuna.stop().await;
    }

    /// Stop the listener and wait for its task to finish. Called on every control-API apply.
    /// Returns whether a listener was actually running, so a restart can be logged as one.
    async fn stop_control_api(&self) -> bool {
        // Token regeneration, disable and rebind all come through here. Close sockets before
        // aborting the server task so clients get a frame instead of a bare TCP reset.
        self.ws.close_all();
        let mut inner = self.inner.lock().await;
        let was_running = inner.server.is_some();
        if let Some(server) = inner.server.take() {
            tracing::info!(addr = %server.addr, "stopping control API");
            let _ = server.shutdown.send(());
            server.task.abort();
            let _ = server.task.await;
        }
        inner.status.running = false;
        inner.status.bind_addr = None;
        inner.status.port = None;
        was_running
    }

    async fn apply(&self, settings: ControlApiSettings, reason: &'static str) {
        let was_running = self.stop_control_api().await;
        let mut status = ControlApiStatus {
            enabled: settings.enabled,
            running: false,
            bind_addr: None,
            port: None,
            last_error: None,
            ws_clients: 0,
        };
        if !settings.enabled {
            self.inner.lock().await.status = status;
            return;
        }

        let ip = match settings.bind.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(e) => {
                status.last_error = Some(format!("invalid bind address: {e}"));
                self.inner.lock().await.status = status;
                return;
            }
        };
        let addr = SocketAddr::new(ip, settings.port);
        if was_running {
            tracing::info!(reason, "restarting control API");
        }
        tracing::info!(%addr, "starting control API");
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let shared = self.shared();
                let (shutdown, rx) = oneshot::channel();
                let task = tokio::spawn(async move {
                    if let Err(e) = serve(listener, router(shared))
                        .with_graceful_shutdown(async move {
                            let _ = rx.await;
                        })
                        .await
                    {
                        tracing::warn!(error = %e, "control API server stopped");
                    }
                });
                status.running = true;
                status.bind_addr = Some(settings.bind.clone());
                status.port = Some(settings.port);
                let mut inner = self.inner.lock().await;
                inner.server = Some(RunningServer { addr, shutdown, task });
                inner.status = status;
                tracing::info!(%addr, "control API listening");
            }
            Err(e) => {
                tracing::warn!(error = %e, "control API bind failed");
                status.last_error = Some(e.to_string());
                self.inner.lock().await.status = status;
            }
        }
    }
}

fn load_settings(db: &Db) -> ControlApiSettings {
    db.get_setting(CONTROL_API_SETTINGS_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn load_tuna_settings(db: &Db) -> TunaSettings {
    db.get_setting(TUNA_SETTINGS_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn load_token(db: &Db) -> String {
    db.get_setting(CONTROL_API_TOKEN_KEY).filter(|token| is_well_formed(token)).unwrap_or_else(
        || {
            let token = generate_token();
            db.set_setting(CONTROL_API_TOKEN_KEY, &token);
            token
        },
    )
}
