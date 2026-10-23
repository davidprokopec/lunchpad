//! The listener: accepts connections, answers plain GETs with the phone page and
//! hands `/ws` upgrades to a [`super::session`]. Stopping cancels every
//! connection and waits for them, so every remote hold is released before
//! [`RunningServer::stop`] returns.

use super::auth::RemoteAuth;
use super::http::{
    host_allowed, origin_allowed, read_head, static_response, AssetSource, HeadError, Response,
};
use super::protocol::RemoteBackend;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Limits {
    /// Open TCP connections at once (page loads and sockets together).
    pub max_connections: usize,
    /// Signed-in phones at once.
    pub max_sessions: usize,
    /// Largest WebSocket message (and frame) a phone may send.
    pub max_message: usize,
    pub head_timeout: Duration,
    /// Time from the upgrade to the `auth`/`pair` message.
    pub auth_timeout: Duration,
    /// Silence (no message, no pong) after which a phone counts as gone.
    pub idle_timeout: Duration,
    pub ping_interval: Duration,
    /// Messages per second a phone may send before it is disconnected.
    pub max_messages_per_second: u32,
    /// Fastest a phone gets new state.
    pub min_push_interval: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 16,
            max_sessions: 4,
            max_message: 4 * 1024,
            head_timeout: Duration::from_secs(5),
            auth_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            ping_interval: Duration::from_secs(10),
            max_messages_per_second: 120,
            min_push_interval: Duration::from_millis(33),
        }
    }
}

/// A signed-in phone, for the Settings list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedRemote {
    pub session: u64,
    pub device_id: String,
    pub name: String,
    pub address: String,
}

struct LiveSession {
    info: ConnectedRemote,
    token: CancellationToken,
}

/// Signed-in phones, so a revoked one can be cut off at once.
#[derive(Default)]
pub struct Sessions {
    next: AtomicU64,
    live: Mutex<HashMap<u64, LiveSession>>,
}

impl Sessions {
    pub fn add(
        &self,
        device_id: &str,
        name: &str,
        address: IpAddr,
        token: CancellationToken,
    ) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let info = ConnectedRemote {
            session: id,
            device_id: device_id.into(),
            name: name.into(),
            address: address.to_string(),
        };
        self.live.lock().insert(id, LiveSession { info, token });
        id
    }

    pub fn remove(&self, id: u64) {
        self.live.lock().remove(&id);
    }

    pub fn count(&self) -> usize {
        self.live.lock().len()
    }

    pub fn list(&self) -> Vec<ConnectedRemote> {
        let mut out: Vec<ConnectedRemote> =
            self.live.lock().values().map(|s| s.info.clone()).collect();
        out.sort_by_key(|s| s.session);
        out
    }

    /// End every session of a device (it was revoked).
    pub fn cancel_device(&self, device_id: &str) {
        for s in self
            .live
            .lock()
            .values()
            .filter(|s| s.info.device_id == device_id)
        {
            s.token.cancel();
        }
    }
}

/// What every connection shares.
pub struct Context {
    pub backend: Arc<dyn RemoteBackend>,
    pub auth: Arc<RemoteAuth>,
    pub assets: Arc<dyn AssetSource>,
    pub sessions: Arc<Sessions>,
    pub limits: Limits,
    /// Something Settings shows changed (a phone signed in or left, a pairing was used).
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

pub struct RunningServer {
    addr: SocketAddr,
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub async fn bind(addr: SocketAddr, ctx: Arc<Context>) -> std::io::Result<RunningServer> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let token = CancellationToken::new();
        let task = tokio::spawn(accept_loop(listener, addr.port(), ctx, token.clone()));
        tracing::info!(%addr, "phone remote listening");
        Ok(RunningServer { addr, token, task })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop without waiting (app exit).
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Stop listening, end every connection and wait until they have released what they held.
    pub async fn stop(self) {
        self.token.cancel();
        let _ = self.task.await;
        tracing::info!(addr = %self.addr, "phone remote stopped");
    }
}

async fn accept_loop(
    listener: TcpListener,
    port: u16,
    ctx: Arc<Context>,
    token: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    if connections.len() >= ctx.limits.max_connections {
                        tracing::debug!(%peer, "phone remote: too many connections, refused");
                        drop(stream);
                        continue;
                    }
                    connections.spawn(connection(stream, peer, port, ctx.clone(), token.child_token()));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "phone remote: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
}

async fn respond(stream: &mut TcpStream, response: Response) {
    let _ = stream.write_all(&response.to_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    port: u16,
    ctx: Arc<Context>,
    token: CancellationToken,
) {
    let _ = stream.set_nodelay(true);
    let head = tokio::select! {
        _ = token.cancelled() => return,
        head = tokio::time::timeout(ctx.limits.head_timeout, read_head(&mut stream)) => head,
    };
    let req = match head {
        Ok(Ok(req)) => req,
        Ok(Err(HeadError::Closed)) => return,
        Ok(Err(HeadError::TooLarge)) => {
            return respond(
                &mut stream,
                Response::text(431, "Request Header Fields Too Large", "request too large"),
            )
            .await
        }
        Ok(Err(_)) => {
            return respond(
                &mut stream,
                Response::text(400, "Bad Request", "bad request"),
            )
            .await
        }
        Err(_) => {
            return respond(
                &mut stream,
                Response::text(408, "Request Timeout", "timeout"),
            )
            .await
        }
    };
    let host = req.header("host").unwrap_or_default().to_string();
    if !host_allowed(&host, port) {
        tracing::debug!(%peer, host = %host, "phone remote: host refused");
        return respond(
            &mut stream,
            Response::text(403, "Forbidden", "host not allowed"),
        )
        .await;
    }
    if req.method != "GET" {
        return respond(
            &mut stream,
            Response::text(405, "Method Not Allowed", "method not allowed"),
        )
        .await;
    }
    let path = req.path.split(['?', '#']).next().unwrap_or_default();
    if path != "/ws" {
        return respond(&mut stream, static_response(&req, ctx.assets.as_ref())).await;
    }
    let Some(key) = req.websocket_key() else {
        return respond(
            &mut stream,
            Response::text(400, "Bad Request", "expected a WebSocket upgrade"),
        )
        .await;
    };
    if !origin_allowed(req.header("origin"), &host) {
        tracing::warn!(%peer, origin = ?req.header("origin"), "phone remote: cross-site WebSocket refused");
        return respond(
            &mut stream,
            Response::text(403, "Forbidden", "origin not allowed"),
        )
        .await;
    }
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let upgrade = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
    if stream.write_all(upgrade.as_bytes()).await.is_err() {
        return;
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(ctx.limits.max_message))
        .max_frame_size(Some(ctx.limits.max_message));
    let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    super::session::run(ws, peer.ip(), ctx, token).await;
}
