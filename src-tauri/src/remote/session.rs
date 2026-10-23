//! One phone's WebSocket: sign-in first, then state pushes and the few commands
//! a remote may send. Whatever the phone holds is released when the socket ends,
//! however it ends (closed, timed out, revoked, server stopped).

use super::protocol::{
    BackendError, ClientMessage, RemoteBackend, ServerMessage, PROTOCOL_VERSION,
};
use super::server::Context;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

/// Close code for a refused sign-in.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// The commands of one signed-in phone and the pads it holds.
pub struct Session {
    pub id: u64,
    backend: Arc<dyn RemoteBackend>,
    held: HashSet<(u8, u8)>,
}

impl Session {
    pub fn new(id: u64, backend: Arc<dyn RemoteBackend>) -> Session {
        Session {
            id,
            backend,
            held: HashSet::new(),
        }
    }

    pub fn held(&self) -> &HashSet<(u8, u8)> {
        &self.held
    }

    /// Handle one message of a signed-in phone; the reply, if any.
    pub fn handle(&mut self, text: &str) -> Option<ServerMessage> {
        let msg = match serde_json::from_str::<ClientMessage>(text) {
            Ok(msg) => msg,
            Err(e) => return Some(ServerMessage::error("badRequest", e.to_string())),
        };
        let failed = |e: BackendError| Some(ServerMessage::error(e.code(), format!("{e:?}")));
        match msg {
            ClientMessage::Press { x, y } => {
                if self.held.contains(&(x, y)) {
                    return None;
                }
                match self.backend.press(self.id, x, y, true) {
                    Ok(true) => {
                        self.held.insert((x, y));
                        None
                    }
                    Ok(false) => Some(ServerMessage::error(
                        "busy",
                        "the pad is held elsewhere or cannot be pressed",
                    )),
                    Err(e) => failed(e),
                }
            }
            ClientMessage::Release { x, y } => {
                if self.held.remove(&(x, y)) {
                    let _ = self.backend.press(self.id, x, y, false);
                }
                None
            }
            ClientMessage::SetFader { fader_id, value } => {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Some(ServerMessage::error(
                        "badRequest",
                        "value must be between 0 and 1",
                    ));
                }
                self.backend
                    .set_fader(&fader_id, value)
                    .err()
                    .and_then(failed)
            }
            ClientMessage::SwitchPage { page_id } => {
                self.backend.switch_page(&page_id).err().and_then(failed)
            }
            ClientMessage::Ping => Some(ServerMessage::Pong),
            ClientMessage::Pair { .. } | ClientMessage::Auth { .. } => Some(ServerMessage::error(
                "alreadyAuthenticated",
                "already signed in",
            )),
        }
    }

    /// Let go of everything this phone still holds.
    pub fn release_all(&mut self) {
        for (x, y) in std::mem::take(&mut self.held) {
            let _ = self.backend.press(self.id, x, y, false);
        }
    }
}

/// How long the goodbye may take; the holds are released before it.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Where writes go and how long each may block: a phone that stops reading must
/// not hold up revoking it, the idle timeout or stopping the server.
struct Out<'a> {
    token: &'a CancellationToken,
    timeout: Duration,
}

impl Out<'_> {
    /// Send one frame; false when it failed, timed out or the connection is being cancelled.
    async fn raw<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        ws: &mut WebSocketStream<S>,
        msg: Message,
    ) -> bool {
        tokio::select! {
            _ = self.token.cancelled() => false,
            sent = tokio::time::timeout(self.timeout, ws.send(msg)) => matches!(sent, Ok(Ok(()))),
        }
    }

    async fn send<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        ws: &mut WebSocketStream<S>,
        msg: &ServerMessage,
    ) -> bool {
        match serde_json::to_string(msg) {
            Ok(text) => self.raw(ws, Message::text(text)).await,
            Err(_) => false,
        }
    }
}

/// Say goodbye, briefly: never more than [`CLOSE_TIMEOUT`].
async fn close<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    code: u16,
    reason: &str,
) {
    let frame = Message::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    }));
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        let _ = ws.send(frame).await;
        let _ = ws.flush().await;
    })
    .await;
}

/// The first text message, skipping pings and pongs; `None` when the socket ended.
async fn first_text<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> Option<String> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(t)) => return Some(t.as_str().to_string()),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}

pub async fn run<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    ip: IpAddr,
    ctx: Arc<Context>,
    token: CancellationToken,
) {
    let out = Out {
        token: &token,
        timeout: ctx.limits.write_timeout,
    };
    let first = tokio::select! {
        _ = token.cancelled() => return,
        first = tokio::time::timeout(ctx.limits.auth_timeout, first_text(&mut ws)) => first,
    };
    let text = match first {
        Ok(Some(text)) => text,
        Ok(None) => return,
        Err(_) => {
            out.send(&mut ws, &ServerMessage::error("timeout", "sign in first"))
                .await;
            return close(&mut ws, CLOSE_UNAUTHORIZED, "timeout").await;
        }
    };
    if ctx.sessions.count() >= ctx.limits.max_sessions {
        return too_many(&mut ws, &out).await;
    }
    let now = Instant::now();
    let signed_in = match serde_json::from_str::<ClientMessage>(&text) {
        Ok(ClientMessage::Pair { v, .. } | ClientMessage::Auth { v, .. })
            if v != PROTOCOL_VERSION =>
        {
            Err((
                "unsupportedVersion",
                "unsupported protocol version".to_string(),
            ))
        }
        Ok(ClientMessage::Pair { code, name, .. }) => {
            let paired = ctx.auth.pair(&code, &name, ip, now);
            if paired.is_ok() {
                (ctx.notify)();
            }
            paired
                .map(|(d, token)| {
                    (
                        d.id.clone(),
                        d.name.clone(),
                        ServerMessage::Paired {
                            v: PROTOCOL_VERSION,
                            device_id: d.id,
                            name: d.name,
                            token,
                        },
                    )
                })
                .map_err(|e| (e.code(), "pairing refused".to_string()))
        }
        Ok(ClientMessage::Auth { token, .. }) => ctx
            .auth
            .authenticate(&token, ip, now)
            .map(|d| {
                (
                    d.id.clone(),
                    d.name.clone(),
                    ServerMessage::AuthOk {
                        v: PROTOCOL_VERSION,
                        device_id: d.id,
                        name: d.name,
                    },
                )
            })
            .map_err(|e| (e.code(), "sign-in refused".to_string())),
        _ => Err(("unauthorized", "sign in first".to_string())),
    };
    let (device_id, name, reply) = match signed_in {
        Ok(ok) => ok,
        Err((code, message)) => {
            tracing::warn!(%ip, code, "phone remote: sign-in refused");
            out.send(&mut ws, &ServerMessage::error(code, message))
                .await;
            return close(&mut ws, CLOSE_UNAUTHORIZED, code).await;
        }
    };
    let session_token = token.child_token();
    let Some(id) = ctx.sessions.try_add(
        &device_id,
        &name,
        ip,
        session_token.clone(),
        ctx.limits.max_sessions,
    ) else {
        return too_many(&mut ws, &out).await;
    };
    // Revoked between the sign-in and the registration above: `cancel_device` missed it.
    if !ctx.auth.is_paired(&device_id) {
        ctx.sessions.remove(id);
        out.send(
            &mut ws,
            &ServerMessage::error("unauthorized", "sign-in refused"),
        )
        .await;
        return close(&mut ws, CLOSE_UNAUTHORIZED, "unauthorized").await;
    }
    (ctx.notify)();
    tracing::info!(session = id, device = %device_id, %ip, "phone remote connected");
    let mut session = Session::new(id, ctx.backend.clone());
    let session_out = Out {
        token: &session_token,
        timeout: ctx.limits.write_timeout,
    };
    let reason = if session_out.send(&mut ws, &reply).await {
        serve(&mut ws, &mut session, &ctx, &session_out).await
    } else {
        "gone"
    };
    session.release_all();
    ctx.sessions.remove(id);
    (ctx.notify)();
    tracing::info!(session = id, reason, "phone remote disconnected");
    close(&mut ws, 1000, reason).await;
}

async fn too_many<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>, out: &Out<'_>) {
    out.send(
        ws,
        &ServerMessage::error("tooManySessions", "too many phones are connected"),
    )
    .await;
    close(ws, 1013, "busy").await;
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    session: &mut Session,
    ctx: &Context,
    out: &Out<'_>,
) -> &'static str {
    let limits = &ctx.limits;
    let token = out.token;
    let mut changes = ctx.backend.changes();
    let mut changes_open = true;
    let mut dirty = true;
    let mut last_state: Option<String> = None;
    let mut next_push = tokio::time::Instant::now();
    let mut last_activity = Instant::now();
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + limits.ping_interval,
        limits.ping_interval,
    );
    let mut window = (Instant::now(), 0u32);
    loop {
        if dirty && tokio::time::Instant::now() >= next_push {
            dirty = false;
            next_push = tokio::time::Instant::now() + limits.min_push_interval;
            let state = ServerMessage::State(Box::new(ctx.backend.state()));
            if let Ok(text) = serde_json::to_string(&state) {
                if last_state.as_deref() != Some(text.as_str()) {
                    if !out.raw(ws, Message::text(text.clone())).await {
                        return if token.is_cancelled() {
                            "stopped"
                        } else {
                            "stuck"
                        };
                    }
                    last_state = Some(text);
                }
            }
        }
        tokio::select! {
            biased;
            _ = token.cancelled() => return "stopped",
            _ = tokio::time::sleep_until(next_push), if dirty => {}
            changed = changes.changed(), if changes_open => match changed {
                Ok(()) => dirty = true,
                Err(_) => changes_open = false,
            },
            _ = ping.tick() => {
                if last_activity.elapsed() >= limits.idle_timeout {
                    out.send(ws, &ServerMessage::error("timeout", "no sign of life")).await;
                    return "timeout";
                }
                if !out.raw(ws, Message::Ping(Vec::new().into())).await {
                    return "stuck";
                }
            }
            msg = ws.next() => match msg {
                None | Some(Err(_)) => return "gone",
                Some(Ok(Message::Close(_))) => return "closed",
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => last_activity = Instant::now(),
                Some(Ok(Message::Text(text))) => {
                    last_activity = Instant::now();
                    if window.0.elapsed() >= std::time::Duration::from_secs(1) {
                        window = (Instant::now(), 0);
                    }
                    window.1 += 1;
                    if window.1 > limits.max_messages_per_second {
                        out.send(ws, &ServerMessage::error("tooManyMessages", "slow down")).await;
                        return "flood";
                    }
                    if let Some(reply) = session.handle(text.as_str()) {
                        if !out.send(ws, &reply).await {
                            return "stuck";
                        }
                    }
                }
                Some(Ok(_)) => {
                    last_activity = Instant::now();
                    if !out.send(ws, &ServerMessage::error("badRequest", "text messages only")).await {
                        return "stuck";
                    }
                }
            },
        }
    }
}
