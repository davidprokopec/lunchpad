//! Phone remote: a page served on the local network that mirrors the Launchpad
//! and presses its pads, for paired phones only. Off unless turned on in Settings.
//!
//! * `pairing`  - codes, tokens, hashing, attempt limits
//! * `auth`     - paired phones in the app settings
//! * `http`     - request heads, Host/Origin checks, the page's files
//! * `protocol` - WebSocket messages and the state a phone draws
//! * `session`  - one phone's socket
//! * `server`   - the listener and its limits
//!
//! Traffic is plain HTTP/WebSocket: anyone on the same network can read it,
//! including a phone's token. Pairing and Host/Origin checks keep other devices
//! and web pages out; they do not encrypt anything.
//!
//! A remote can only press and release pads, move the faders of the page on
//! screen and switch pages, through the same paths as the hardware. Pressing a
//! pad runs whatever actions the user configured on it (which may include
//! scripts, programs or HTTP requests); nothing else is reachable.

pub mod auth;
pub mod http;
pub mod pairing;
pub mod protocol;
pub mod server;
pub mod session;

use crate::config::SharedSettings;
use crate::macros::MacroEngine;
use crate::midi::SharedManager;
use crate::profile::ProfileStore;
use auth::RemoteAuth;
use http::AssetSource;
use parking_lot::Mutex;
use protocol::{build_state, fader_control, BackendError, RemoteBackend, RemoteState};
use serde::Serialize;
use server::{ConnectedRemote, Context, Limits, RunningServer, Sessions};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

pub const EVENT_STATUS: &str = "remote:status";

const BIND_RETRIES: usize = 10;
const BIND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Lowest port Settings accepts (below are privileged ports).
pub const MIN_PORT: u16 = 1024;

/// The app's remote backend: presses go through the device manager like the
/// hardware's, faders and pages through the macro engine.
pub struct AppBackend {
    manager: SharedManager,
    engine: MacroEngine,
    profile: Arc<Mutex<ProfileStore>>,
    changes: tokio::sync::watch::Receiver<u64>,
}

impl AppBackend {
    pub fn new(
        manager: SharedManager,
        engine: MacroEngine,
        profile: Arc<Mutex<ProfileStore>>,
    ) -> AppBackend {
        let changes = manager.lock().frame_ticks();
        AppBackend {
            manager,
            engine,
            profile,
            changes,
        }
    }
}

impl RemoteBackend for AppBackend {
    fn state(&self) -> RemoteState {
        let view = self.manager.lock().render_view();
        build_state(view.as_ref(), &self.profile)
    }

    fn press(&self, session: u64, x: u8, y: u8, pressed: bool) -> Result<bool, BackendError> {
        self.manager
            .lock()
            .remote_press(session, x, y, pressed)
            .map_err(|_| BackendError::NoDevice)
    }

    fn set_fader(&self, fader_id: &str, fraction: f64) -> Result<(), BackendError> {
        if !self.manager.lock().is_connected() {
            return Err(BackendError::NoDevice);
        }
        let event = fader_control(&self.profile, fader_id, fraction)?;
        self.engine.on_control(&event);
        Ok(())
    }

    fn switch_page(&self, page_id: &str) -> Result<(), BackendError> {
        if !self.engine.switch_page(page_id) {
            return Err(BackendError::UnknownPage);
        }
        // With a Launchpad the repaint announces it; without one nothing paints.
        let manager = self.manager.lock();
        if !manager.is_connected() {
            manager.notify_view_changed();
        }
        Ok(())
    }

    fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.clone()
    }
}

/// The page's files from the app's bundled frontend.
pub struct TauriAssets(pub tauri::AppHandle);

impl AssetSource for TauriAssets {
    fn get(&self, path: &str) -> Option<(Vec<u8>, String)> {
        // The resolver answers a missing file with the desktop app's `index.html`;
        // only files that exist are served. (Embedded assets are keyed `/<path>`;
        // a dev build embeds none and reads the dist folder without that fallback.)
        let resolver = self.0.asset_resolver();
        let key = format!("/{path}");
        let mut embedded = resolver.iter().map(|(k, _)| k).peekable();
        if embedded.peek().is_some() && !embedded.any(|k| k == key.as_str()) {
            return None;
        }
        self.0
            .asset_resolver()
            .get(path.to_string())
            .map(|a| (a.bytes, a.mime_type))
    }
}

/// The address other devices reach this machine at: the interface of the default
/// route (a UDP "connect" sends nothing, it only picks the route).
pub fn lan_addresses() -> Vec<IpAddr> {
    let probe = || -> std::io::Result<IpAddr> {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
        Ok(socket.local_addr()?.ip())
    };
    match probe() {
        Ok(ip) if !ip.is_unspecified() && !ip.is_loopback() => vec![ip],
        _ => Vec::new(),
    }
}

/// `<host>.local`, when the machine name can be one.
fn mdns_name() -> Option<String> {
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_ascii_lowercase();
    let name = name.strip_suffix(".local").unwrap_or(&name).to_string();
    (!name.is_empty()
        && !name.contains('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    .then(|| format!("{name}.local"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDeviceStatus {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: Option<i64>,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePairing {
    /// The code to type on the phone.
    pub code: String,
    /// What the QR code holds: the page with the code in the fragment (never sent to the server).
    pub url: Option<String>,
    /// Unix milliseconds
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub enabled: bool,
    pub port: u16,
    /// Listening right now.
    pub running: bool,
    /// Why it is not listening although enabled (e.g. the port is taken).
    pub error: Option<String>,
    pub addresses: Vec<String>,
    /// Where phones open the page, best first.
    pub urls: Vec<String>,
    pub pairing: Option<RemotePairing>,
    pub devices: Vec<RemoteDeviceStatus>,
    pub connected: Vec<ConnectedRemote>,
}

pub type StatusListener = Arc<dyn Fn(&RemoteStatus) + Send + Sync>;

pub struct Options {
    pub bind_ip: IpAddr,
    pub addresses: Arc<dyn Fn() -> Vec<IpAddr> + Send + Sync>,
    pub mdns: bool,
    pub limits: Limits,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            addresses: Arc::new(lan_addresses),
            mdns: true,
            limits: Limits::default(),
        }
    }
}

struct Running {
    server: RunningServer,
    port: u16,
}

struct Inner {
    settings: SharedSettings,
    ctx: Arc<Context>,
    options: Options,
    server: Mutex<Option<Running>>,
    /// Bumped by every `apply_settings`; an older call that sees a newer one gives way.
    generation: AtomicU64,
    /// The port listened on, readable without the async lock.
    running_port: Mutex<Option<u16>>,
    error: Mutex<Option<String>>,
    /// The code currently shown, kept for the status.
    code: Mutex<Option<String>>,
    listener: Mutex<Option<StatusListener>>,
}

/// Starts, stops and reports the phone remote.
#[derive(Clone)]
pub struct RemoteService {
    inner: Arc<Inner>,
}

impl RemoteService {
    pub fn new(
        settings: SharedSettings,
        backend: Arc<dyn RemoteBackend>,
        assets: Arc<dyn AssetSource>,
        options: Options,
    ) -> RemoteService {
        let inner = Arc::new_cyclic(|weak: &Weak<Inner>| {
            let weak = weak.clone();
            let ctx = Arc::new(Context {
                backend,
                auth: Arc::new(RemoteAuth::new(settings.clone())),
                assets,
                sessions: Arc::new(Sessions::default()),
                limits: options.limits.clone(),
                notify: Arc::new(move || {
                    if let Some(inner) = weak.upgrade() {
                        RemoteService { inner }.publish();
                    }
                }),
            });
            Inner {
                settings,
                ctx,
                options,
                server: Mutex::new(None),
                generation: AtomicU64::new(0),
                running_port: Mutex::new(None),
                error: Mutex::new(None),
                code: Mutex::new(None),
                listener: Mutex::new(None),
            }
        });
        RemoteService { inner }
    }

    /// Called with the new status whenever it changes.
    pub fn set_listener(&self, listener: StatusListener) {
        *self.inner.listener.lock() = Some(listener);
    }

    fn publish(&self) {
        let listener = self.inner.listener.lock().clone();
        if let Some(listener) = listener {
            listener(&self.status());
        }
    }

    pub fn status(&self) -> RemoteStatus {
        let config = self.inner.settings.lock().settings.remote.clone();
        let running_port = *self.inner.running_port.lock();
        let addresses: Vec<IpAddr> = if running_port.is_some() {
            (self.inner.options.addresses)()
        } else {
            Vec::new()
        };
        let urls = running_port
            .map(|port| self.urls(&addresses, port))
            .unwrap_or_default();
        let now = Instant::now();
        let pairing = self.inner.ctx.auth.pending(now).and_then(|expires| {
            let code = self.inner.code.lock().clone()?;
            let expires_at = chrono::Utc::now().timestamp_millis()
                + expires.saturating_duration_since(now).as_millis() as i64;
            Some(RemotePairing {
                url: urls.first().map(|u| format!("{u}#pair={code}")),
                code,
                expires_at,
            })
        });
        let connected = self.inner.ctx.sessions.list();
        RemoteStatus {
            enabled: config.enabled,
            port: config.port,
            running: running_port.is_some(),
            error: self.inner.error.lock().clone(),
            addresses: addresses.iter().map(|a| a.to_string()).collect(),
            urls,
            pairing,
            devices: config
                .devices
                .iter()
                .map(|d| RemoteDeviceStatus {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    created_at: d.created_at,
                    last_seen: d.last_seen,
                    connected: connected.iter().any(|c| c.device_id == d.id),
                })
                .collect(),
            connected,
        }
    }

    fn urls(&self, addresses: &[IpAddr], port: u16) -> Vec<String> {
        let mut urls: Vec<String> = addresses
            .iter()
            .map(|ip| format!("http://{}/", SocketAddr::new(*ip, port)))
            .collect();
        if self.inner.options.mdns {
            if let Some(name) = mdns_name() {
                urls.push(format!("http://{name}:{port}/"));
            }
        }
        urls
    }

    /// Listen or not, as the settings say. Restarts when the port changed.
    pub async fn apply_settings(&self) -> RemoteStatus {
        let config = self.inner.settings.lock().settings.remote.clone();
        let wanted = config.enabled.then_some(config.port);
        // Locks are only held for bookkeeping, never across an await. A newer call
        // takes over from an older one still stopping or binding (`generation`).
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let current = || self.inner.generation.load(Ordering::SeqCst) == generation;
        let old = {
            let mut server = self.inner.server.lock();
            if matches!((&*server, wanted), (Some(r), Some(port)) if r.port == port) {
                drop(server);
                self.publish();
                return self.status();
            }
            *self.inner.running_port.lock() = None;
            *self.inner.error.lock() = None;
            if !config.enabled {
                self.cancel_pairing_quiet();
            }
            server.take()
        };
        if let Some(old) = old {
            old.server.stop().await;
        }
        if let Some(port) = wanted.filter(|_| current()) {
            let addr = SocketAddr::new(self.inner.options.bind_ip, port);
            let mut bound = RunningServer::bind(addr, self.inner.ctx.clone()).await;
            // An older call may still be letting go of the same port.
            for _ in 0..BIND_RETRIES {
                match &bound {
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && current() => {
                        tokio::time::sleep(BIND_RETRY_DELAY).await;
                        bound = RunningServer::bind(addr, self.inner.ctx.clone()).await;
                    }
                    _ => break,
                }
            }
            match bound {
                Ok(running) => {
                    let mut server = self.inner.server.lock();
                    if current() && server.is_none() {
                        *self.inner.running_port.lock() = Some(running.port());
                        *server = Some(Running {
                            server: running,
                            port,
                        });
                    } else {
                        running.cancel();
                    }
                }
                Err(e) if current() => {
                    tracing::warn!(port, error = %e, "phone remote could not listen");
                    *self.inner.error.lock() =
                        Some(format!("Port {port} could not be opened: {e}"));
                }
                Err(_) => {}
            }
        }
        self.publish();
        self.status()
    }

    /// Settings → Remote: turn it on or off and choose the port.
    pub async fn configure(&self, enabled: bool, port: u16) -> Result<RemoteStatus, String> {
        if port < MIN_PORT {
            return Err(format!("The port must be between {MIN_PORT} and 65535"));
        }
        {
            let mut store = self.inner.settings.lock();
            let before = store.settings.remote.clone();
            store.settings.remote.enabled = enabled;
            store.settings.remote.port = port;
            if let Err(e) = store.save() {
                store.settings.remote = before;
                return Err(e.to_string());
            }
        }
        Ok(self.apply_settings().await)
    }

    /// Show a pairing code (replacing any earlier one); only while listening.
    pub fn start_pairing(&self) -> Result<RemotePairing, String> {
        if self.inner.running_port.lock().is_none() {
            return Err("Turn the phone remote on first".into());
        }
        let (code, _) = self.inner.ctx.auth.start_pairing(Instant::now());
        *self.inner.code.lock() = Some(code);
        self.publish();
        self.status()
            .pairing
            .ok_or_else(|| "pairing could not start".to_string())
    }

    fn cancel_pairing_quiet(&self) {
        self.inner.ctx.auth.cancel_pairing();
        *self.inner.code.lock() = None;
    }

    pub fn cancel_pairing(&self) -> RemoteStatus {
        self.cancel_pairing_quiet();
        self.publish();
        self.status()
    }

    /// Forget a phone and disconnect it.
    pub fn revoke(&self, device_id: &str) -> Result<RemoteStatus, String> {
        self.inner.ctx.auth.revoke(device_id)?;
        self.inner.ctx.sessions.cancel_device(device_id);
        self.publish();
        Ok(self.status())
    }

    /// App exit: stop listening and end every connection without waiting.
    pub fn shutdown(&self) {
        if let Some(running) = self.inner.server.lock().as_ref() {
            running.server.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
