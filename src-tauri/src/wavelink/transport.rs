use super::catalog::{Catalog, Domain};
use super::*;
use crate::config::WaveLinkSettings;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const RPC_TIMEOUT: Duration = Duration::from_secs(3);
const PENDING_LIMIT: usize = 32;
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Reply = crossbeam_channel::Sender<Result<(), String>>;

#[derive(Clone)]
struct Desired {
    config: WaveLinkSettings,
    connect: bool,
    shutdown: bool,
    epoch: u64,
}

struct Inner {
    state: Mutex<WaveLinkState>,
    changes: watch::Sender<WaveLinkState>,
    desired: watch::Sender<Desired>,
    epoch: AtomicU64,
    commands: mpsc::Sender<Command>,
}

#[derive(Clone)]
pub struct WaveLink {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone)]
pub(crate) enum Mutation {
    Level(WaveLinkTarget, f64),
    Mute(WaveLinkTarget, bool),
    MainOutput(WaveLinkMainOutput),
    MonitoredMix(WaveLinkMainOutput, String),
}

enum Command {
    Write {
        generation: u64,
        mutation: Mutation,
        reply: Reply,
    },
    Refresh,
}

impl WaveLink {
    pub fn start(config: WaveLinkSettings) -> Self {
        Self::start_with_ports(config, None)
    }

    pub(super) fn start_with_ports(config: WaveLinkSettings, ports: Option<Vec<u16>>) -> Self {
        #[cfg(test)]
        if let Some(ports) = &ports {
            assert!(
                ports
                    .iter()
                    .all(|port| *port != 0 && !(1884..=1893).contains(port)),
                "tests must use ephemeral fixture ports, never installed Wave Link ports"
            );
        }
        let (commands, receiver) = mpsc::channel(PENDING_LIMIT);
        let invalid = config.validate().err();
        let connect = config.enabled && config.auto_connect && invalid.is_none();
        let (desired, control) = watch::channel(Desired {
            config,
            connect,
            shutdown: false,
            epoch: 0,
        });
        let initial = if let Some(error) = invalid {
            WaveLinkState {
                status: WaveLinkStatus::Error,
                error: Some(error),
                ..Default::default()
            }
        } else {
            WaveLinkState::default()
        };
        let (changes, _) = watch::channel(initial.clone());
        let inner = Arc::new(Inner {
            state: Mutex::new(initial),
            changes,
            desired,
            epoch: AtomicU64::new(0),
            commands,
        });
        let worker = inner.clone();
        tauri::async_runtime::spawn(async move {
            supervise(worker, control, receiver, ports).await;
        });
        Self { inner }
    }

    pub fn state(&self) -> WaveLinkState {
        self.inner.state.lock().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<WaveLinkState> {
        self.inner.changes.subscribe()
    }
    pub fn configure(&self, config: WaveLinkSettings) -> Result<(), String> {
        config.validate()?;
        self.change(Some(config), false, false);
        Ok(())
    }
    pub fn connect(&self) -> WaveLinkState {
        if self.inner.desired.borrow().config.validate().is_err() {
            return self.state();
        }
        if self.inner.desired.borrow().connect {
            return self.state();
        }
        self.change(None, true, false);
        self.state()
    }
    pub fn disconnect(&self) -> WaveLinkState {
        self.change(None, false, false);
        self.state()
    }
    pub fn shutdown(&self) {
        self.change(None, false, true);
    }
    pub fn refresh(&self) -> WaveLinkState {
        let _ = self.inner.commands.try_send(Command::Refresh);
        self.state()
    }

    fn change(&self, config: Option<WaveLinkSettings>, connect: bool, shutdown: bool) {
        self.inner.desired.send_if_modified(|desired| {
            if desired.shutdown {
                return false;
            }
            if let Some(config) = config {
                if desired.config == config {
                    return false;
                }
                desired.connect = config.enabled
                    && config.auto_connect
                    && (!desired.config.enabled || desired.connect);
                desired.config = config;
            } else {
                if connect && desired.connect && !shutdown {
                    return false;
                }
                desired.connect = connect && desired.config.enabled;
            }
            desired.shutdown = shutdown;
            desired.epoch = self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let mut state = self.inner.state.lock();
            let status = if !desired.config.enabled || shutdown {
                WaveLinkStatus::Disabled
            } else if desired.connect {
                WaveLinkStatus::Connecting
            } else {
                WaveLinkStatus::Disconnected
            };
            let generation = state.generation + 1;
            let revision = state.revision + 1;
            *state = WaveLinkState {
                status,
                generation,
                revision,
                ..Default::default()
            };
            self.inner.changes.send_replace(state.clone());
            true
        });
    }

    pub(crate) fn write(&self, generation: u64, mutation: Mutation) -> Result<(), String> {
        let state = self.state();
        if state.status != WaveLinkStatus::Ready || state.generation != generation {
            return Err("Wave Link is offline or the captured session has ended".into());
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.inner
            .commands
            .try_send(Command::Write {
                generation,
                mutation,
                reply,
            })
            .map_err(|_| "Wave Link request queue is full or stopped".to_string())?;
        response
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| {
                "Wave Link write/readback did not complete; result unconfirmed".to_string()
            })?
    }
}

impl Inner {
    fn publish(&self, epoch: u64, update: impl FnOnce(&mut WaveLinkState)) {
        let mut state = self.state.lock();
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        let before = state.clone();
        update(&mut state);
        if *state != before {
            state.revision = before.revision + 1;
            self.changes.send_replace(state.clone());
        }
    }
}

fn discovery(config: &WaveLinkSettings) -> Vec<u16> {
    if let Some(port) = config.port {
        return vec![port];
    }
    let mut ports = Vec::new();
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        let path = std::path::PathBuf::from(root)
            .join("Packages/Elgato.WaveLink_g54w8ztgkx496/LocalState/ws-info.json");
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(port) = value
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p != 0)
                {
                    ports.push(port);
                }
            }
        }
    }
    for port in 1884..=1893 {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

async fn identify(
    port: u16,
    inner: &Inner,
    epoch: u64,
) -> Result<(Socket, WaveLinkApplication, Option<String>), String> {
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .map_err(|error| error.to_string())?;
    request.headers_mut().insert(
        "Origin",
        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("streamdeck://"),
    );
    let (mut socket, _) = timeout(
        Duration::from_secs(1),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "Wave Link candidate connection timed out".to_string())?
    .map_err(|error| error.to_string())?;
    inner.publish(epoch, |state| {
        state.connected = true;
        state.status = WaveLinkStatus::Synchronizing;
    });
    timeout(RPC_TIMEOUT, async {
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":1,"method":"getApplicationInfo"})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| error.to_string())?;
        loop {
            let message = socket
                .next()
                .await
                .ok_or("Wave Link closed during identification")?
                .map_err(|error| error.to_string())?;
            if let Message::Text(text) = message {
                let value: Value =
                    serde_json::from_str(text.as_str()).map_err(|error| error.to_string())?;
                if value.get("id").and_then(Value::as_u64) != Some(1) {
                    continue;
                }
                if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                    return Err("invalid JSON-RPC identification".into());
                }
                let result = value
                    .get("result")
                    .ok_or("Wave Link identification failed")?;
                let app: WaveLinkApplication =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if app.app_id != "EWL" || app.interface_revision != 2 {
                    return Err("unsupported Wave Link identity/interface revision".into());
                }
                let version = result
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                return Ok((socket, app, version));
            }
        }
    })
    .await
    .map_err(|_| "Wave Link identification timed out".to_string())?
}

async fn supervise(
    inner: Arc<Inner>,
    mut desired: watch::Receiver<Desired>,
    mut commands: mpsc::Receiver<Command>,
    fixture_ports: Option<Vec<u16>>,
) {
    loop {
        let config = desired.borrow_and_update().clone();
        if config.shutdown {
            return;
        }
        if !config.connect {
            inner.publish(config.epoch, |state| {
                state.status = if matches!(
                    state.status,
                    WaveLinkStatus::Unsupported | WaveLinkStatus::Error
                ) && config.config.enabled
                {
                    state.status
                } else if config.config.enabled {
                    WaveLinkStatus::Disconnected
                } else {
                    WaveLinkStatus::Disabled
                }
            });
            tokio::select! { _ = desired.changed() => {}, command = commands.recv() => {
                match command { Some(Command::Write { reply, .. }) => { let _ = reply.send(Err("Wave Link is disabled/disconnected".into())); }, Some(Command::Refresh) => {}, None => return }
            }}
            continue;
        }
        let attempt = async {
            let mut backoff = 1;
            loop {
                let mut last_error = "Wave Link was not found".to_string();
                #[cfg(test)]
                assert!(
                    fixture_ports.is_some(),
                    "networked Wave Link tests require mock discovery ports"
                );
                let ports = fixture_ports
                    .clone()
                    .unwrap_or_else(|| discovery(&config.config));
                for port in ports {
                    inner.publish(config.epoch, |state| {
                        let revision = state.revision;
                        let generation = state.generation + 1;
                        *state = WaveLinkState {
                            status: WaveLinkStatus::Connecting,
                            port: Some(port),
                            generation,
                            revision,
                            ..Default::default()
                        };
                    });
                    match identify(port, &inner, config.epoch).await {
                        Ok((socket, app, version)) => {
                            tracing::info!(port, "Wave Link identified");
                            inner.publish(config.epoch, |state| {
                                state.connected = true;
                                state.status = WaveLinkStatus::Synchronizing;
                                state.application = Some(app);
                                state.version = version;
                            });
                            match session(&inner, config.epoch, socket, &mut commands).await {
                                Ok(()) => return,
                                Err(error) => {
                                    if inner.state.lock().status == WaveLinkStatus::Ready {
                                        backoff = 1;
                                    }
                                    last_error = error;
                                }
                            }
                            break;
                        }
                        Err(error) => {
                            tracing::debug!(port, %error, "Wave Link candidate rejected");
                            last_error = error;
                        }
                    }
                }
                tracing::warn!(error = %last_error, "Wave Link disconnected");
                inner.publish(config.epoch, |state| {
                    let generation = state.generation + 1;
                    let revision = state.revision;
                    *state = WaveLinkState {
                        status: if last_error.contains("unsupported Wave Link") {
                            WaveLinkStatus::Unsupported
                        } else {
                            WaveLinkStatus::Disconnected
                        },
                        error: Some(last_error),
                        generation,
                        revision,
                        ..Default::default()
                    };
                });
                while let Ok(command) = commands.try_recv() {
                    if let Command::Write { reply, .. } = command {
                        let _ = reply.send(Err(
                            "Wave Link session ended; writes are not replayed".into()
                        ));
                    }
                }
                if !config.config.auto_connect {
                    inner.desired.send_modify(|desired| {
                        if desired.epoch == config.epoch {
                            desired.connect = false;
                        }
                    });
                    return;
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        };
        tokio::select! { biased; _ = desired.changed() => {}, _ = attempt => {} }
    }
}

enum Purpose {
    Read {
        domain: Domain,
        fence: u64,
        retries: u8,
        reply: Option<Reply>,
    },
    Write {
        domain: Domain,
        reply: Reply,
    },
}
struct Pending {
    purpose: Purpose,
    deadline: Instant,
}

struct Session {
    catalog: Catalog,
    unsupported: HashMap<Domain, String>,
    versions: HashMap<Domain, u64>,
    pending: HashMap<u64, Pending>,
    reads: HashMap<Domain, u64>,
    next_id: u64,
}

impl Session {
    fn bump(&mut self, domain: Domain) {
        *self.versions.entry(domain).or_default() += 1;
    }
    async fn send(
        &mut self,
        socket: &mut Socket,
        method: &str,
        params: Option<Value>,
        purpose: Purpose,
    ) -> Result<u64, String> {
        if self.pending.len() >= PENDING_LIMIT {
            return Err("Wave Link pending request limit reached".into());
        }
        self.next_id += 1;
        let id = self.next_id;
        let deadline = Instant::now() + RPC_TIMEOUT;
        let mut request = json!({"jsonrpc":"2.0","id":id,"method":method});
        if let Some(params) = params {
            request["params"] = params;
        }
        tokio::time::timeout_at(
            deadline,
            socket.send(Message::Text(request.to_string().into())),
        )
        .await
        .map_err(|_| "Wave Link send timed out; result unconfirmed".to_string())?
        .map_err(|error| error.to_string())?;
        self.pending.insert(id, Pending { purpose, deadline });
        Ok(id)
    }
    async fn read(
        &mut self,
        socket: &mut Socket,
        domain: Domain,
        retries: u8,
        reply: Option<Reply>,
    ) -> Result<(), String> {
        if let Some(id) = self.reads.remove(&domain) {
            if reply.is_none() {
                self.reads.insert(domain, id);
                return Ok(());
            }
            self.pending.remove(&id);
        }
        let fence = *self.versions.entry(domain).or_default();
        let id = self
            .send(
                socket,
                domain.method(),
                None,
                Purpose::Read {
                    domain,
                    fence,
                    retries,
                    reply,
                },
            )
            .await?;
        self.reads.insert(domain, id);
        Ok(())
    }
    fn publish(&self, inner: &Inner, epoch: u64) {
        inner.publish(epoch, |state| {
            state.status = if !self.unsupported.is_empty() {
                WaveLinkStatus::Unsupported
            } else if self.catalog.complete() {
                WaveLinkStatus::Ready
            } else {
                WaveLinkStatus::Synchronizing
            };
            self.catalog.apply(state);
            if let Some(error) = Domain::ALL
                .iter()
                .find_map(|domain| self.unsupported.get(domain))
            {
                state.error = Some(error.clone());
            } else if self.catalog.complete() {
                state.error = None;
            }
        });
    }
}

async fn session(
    inner: &Inner,
    epoch: u64,
    mut socket: Socket,
    commands: &mut mpsc::Receiver<Command>,
) -> Result<(), String> {
    let mut session = Session {
        catalog: Catalog::default(),
        unsupported: HashMap::new(),
        versions: HashMap::new(),
        pending: HashMap::new(),
        reads: HashMap::new(),
        next_id: 1,
    };
    for domain in Domain::ALL {
        session.read(&mut socket, domain, 0, None).await?;
    }
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;
    let mut deadlines = tokio::time::interval(Duration::from_millis(50));
    deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Refresh) => for domain in Domain::ALL { session.read(&mut socket, domain, 0, None).await?; },
                Some(Command::Write { generation, mutation, reply }) => {
                    let state = inner.state.lock().clone();
                    let write = if generation != state.generation { Err("Wave Link captured session ended".into()) } else { mutation_request(&state, &mutation) };
                    match write {
                        Ok((domain,method,params)) => {
                            session.bump(domain);
                            if let Some(id) = session.reads.remove(&domain) { session.pending.remove(&id); }
                            session.send(&mut socket, method, Some(params), Purpose::Write { domain, reply }).await?;
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }, None => return Ok(()),
            },
            _ = poll.tick() => for domain in Domain::ALL { session.read(&mut socket, domain, 0, None).await?; },
            _ = deadlines.tick() => {
                let expired: Vec<u64> = session.pending.iter().filter(|(_,pending)| pending.deadline <= Instant::now()).map(|(id,_)| *id).collect();
                for id in expired {
                    if let Some(pending) = session.pending.remove(&id) {
                        let (domain,reply) = match pending.purpose {
                            Purpose::Read { domain, reply, .. } => { session.reads.remove(&domain); (domain, reply) },
                            Purpose::Write { domain, reply } => (domain, Some(reply)),
                        };
                        if let Some(reply) = reply { let _ = reply.send(Err("Wave Link request timed out; result unconfirmed, not retried".into())); }
                        session.catalog.invalidate(domain);
                        session.bump(domain);
                        session.publish(inner, epoch);
                        inner.publish(epoch, |state| state.error = Some("Wave Link request timed out; result unconfirmed".into()));
                        session.read(&mut socket, domain, 0, None).await?;
                    }
                }
            },
            message = socket.next() => {
                let message = message.ok_or("Wave Link socket closed")?.map_err(|error| error.to_string())?;
                let Message::Text(text) = message else { if message.is_close() { return Err("Wave Link socket closed".into()); } continue; };
                let value: Value = serde_json::from_str(text.as_str()).map_err(|error| error.to_string())?;
                if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") { return Err("Wave Link sent invalid JSON-RPC".into()); }
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    if session.pending.get(&id).is_some_and(|pending| pending.deadline <= Instant::now()) { continue; }
                    let Some(pending) = session.pending.remove(&id) else { continue; };
                    match pending.purpose {
                        Purpose::Write { domain, reply } => {
                            if let Some(error) = value.get("error") {
                                let code=error.get("code").and_then(Value::as_i64);
                                let message=error.get("message").and_then(Value::as_str).unwrap_or("mutation rejected");
                                let _ = reply.send(Err(format!("Wave Link rejected mutation ({code:?}): {message}")));
                                session.catalog.invalidate(domain); session.bump(domain); session.publish(inner,epoch);
                                session.read(&mut socket, domain, 0, None).await?;
                            } else if value.get("result").is_some() { session.read(&mut socket,domain,0,Some(reply)).await?; }
                            else { let _ = reply.send(Err("Wave Link mutation response missing result".into())); return Err("invalid Wave Link mutation response".into()); }
                        }
                        Purpose::Read { domain, fence, retries, reply } => {
                            session.reads.remove(&domain);
                            if session.versions.get(&domain).copied().unwrap_or_default() != fence {
                                if retries < 2 { session.read(&mut socket,domain,retries+1,reply).await?; }
                                else {
                                    if let Some(reply) = reply { let _=reply.send(Err("Wave Link state changed during readback; resynchronizing".into())); }
                                    session.catalog.invalidate(domain); session.publish(inner,epoch); session.read(&mut socket,domain,0,None).await?;
                                }
                            } else {
                                let result = value.get("result").cloned().ok_or("Wave Link getter failed".to_string()).and_then(|value| session.catalog.replace(domain,value));
                                match &result {
                                    Ok(()) => {session.unsupported.remove(&domain);},
                                    Err(error) => {
                                        if session.unsupported.get(&domain)!=Some(error) {tracing::warn!(?domain,%error,"Wave Link catalog unsupported; writes disabled");}
                                        session.catalog.invalidate(domain);session.unsupported.insert(domain,error.clone());
                                    },
                                }
                                session.publish(inner,epoch);
                                if let Some(reply) = reply { let result=if inner.epoch.load(Ordering::Acquire)==epoch {result} else {Err("Wave Link session ended during readback".into())}; let _=reply.send(result); }
                            }
                        }
                    }
                } else if let Some((domain,plural)) = value.get("method").and_then(Value::as_str).and_then(Domain::notification) {
                    session.bump(domain);
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let result = if plural {session.catalog.replace(domain,params)} else {session.catalog.patch(domain,params)};
                    if plural && result.is_ok() {session.unsupported.remove(&domain);}
                    if let Err(error)=&result { tracing::warn!(?domain,%error,"Wave Link notification malformed; resynchronizing"); session.catalog.invalidate(domain); }
                    session.publish(inner,epoch);
                    if result.is_err() {session.read(&mut socket,domain,0,None).await?;}
                }
            }
        }
    }
}

fn mutation_request(
    state: &WaveLinkState,
    mutation: &Mutation,
) -> Result<(Domain, &'static str, Value), String> {
    if state.status != WaveLinkStatus::Ready {
        return Err("Wave Link is not ready".into());
    }
    let (target, field, value) = match mutation {
        Mutation::Level(target, value) => {
            if !value.is_finite() || !(0.0..=1.0).contains(value) {
                return Err("Wave Link level must be between 0 and 1".into());
            }
            let control = state
                .control(target)
                .ok_or("Wave Link target was not discovered")?;
            if !control.can_set_level {
                return Err("Wave Link level control is unsupported".into());
            }
            (target, "level", json!(value))
        }
        Mutation::Mute(target, value) => {
            let control = state
                .control(target)
                .ok_or("Wave Link target was not discovered")?;
            if !control.can_mute {
                return Err("Wave Link mute control is unsupported".into());
            }
            (target, "isMuted", json!(value))
        }
        Mutation::MainOutput(target) | Mutation::MonitoredMix(target, _) => {
            let output = state
                .outputs
                .iter()
                .find(|output| {
                    output.output_device_id == target.output_device_id
                        && output.output_id == target.output_id
                })
                .ok_or("Wave Link output was not discovered")?;
            if let Mutation::MonitoredMix(_, mix_id) = mutation {
                if !output.can_set_mix
                    || mix_id.is_empty()
                    || !state.mixes.iter().any(|mix| &mix.id == mix_id)
                {
                    return Err("Wave Link monitored mix is unsupported/undiscovered".into());
                }
                return Ok((
                    Domain::Outputs,
                    "setOutputDevice",
                    json!({"outputDevice":{"id":target.output_device_id,"outputs":[{"id":target.output_id,"mixId":mix_id}]}}),
                ));
            }
            if !output.can_select_main {
                return Err("Wave Link output selection is unsupported".into());
            }
            return Ok((
                Domain::Outputs,
                "setOutputDevice",
                json!({"mainOutput":target}),
            ));
        }
    };
    let mut change = json!({});
    change[field] = value;
    Ok(match target {
        WaveLinkTarget::Channel { channel_id } => {
            change["id"] = json!(channel_id);
            (Domain::Channels, "setChannel", change)
        }
        WaveLinkTarget::ChannelMix { channel_id, mix_id } => {
            change["id"] = json!(mix_id);
            (
                Domain::Channels,
                "setChannel",
                json!({"id":channel_id,"mixes":[change]}),
            )
        }
        WaveLinkTarget::Mix { mix_id } => {
            change["id"] = json!(mix_id);
            (Domain::Mixes, "setMix", change)
        }
        WaveLinkTarget::Input {
            input_device_id,
            input_id,
        } => {
            change["id"] = json!(input_id);
            (
                Domain::Inputs,
                "setInputDevice",
                json!({"id":input_device_id,"inputs":[change]}),
            )
        }
        WaveLinkTarget::Output {
            output_device_id,
            output_id,
        } => {
            change["id"] = json!(output_id);
            (
                Domain::Outputs,
                "setOutputDevice",
                json!({"outputDevice":{"id":output_device_id,"outputs":[change]}}),
            )
        }
        WaveLinkTarget::InputGain { .. } => {
            return Err("Wave Link input gain mapping is unvalidated and read-only".into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_fixed_loopback_and_explicit_port_wins() {
        assert_eq!(
            discovery(&WaveLinkSettings {
                port: Some(12345),
                ..Default::default()
            }),
            vec![12345]
        );
        #[cfg(not(windows))]
        assert_eq!(
            discovery(&WaveLinkSettings::default()),
            (1884..=1893).collect::<Vec<_>>()
        );
    }

    #[test]
    fn mutation_schemas_are_qualified_and_gain_and_unassigned_mix_writes_are_rejected() {
        let mut catalog = Catalog::default();
        for domain in Domain::ALL {
            catalog
                .replace(domain, super::super::tests::catalog(domain.method()))
                .unwrap();
        }
        let mut state = WaveLinkState {
            status: WaveLinkStatus::Ready,
            ..Default::default()
        };
        catalog.apply(&mut state);
        let target = WaveLinkTarget::ChannelMix {
            channel_id: "channel".into(),
            mix_id: "mix".into(),
        };
        let (_, method, params) = mutation_request(&state, &Mutation::Level(target, 0.2)).unwrap();
        assert_eq!(method, "setChannel");
        assert_eq!(
            params,
            json!({"id":"channel","mixes":[{"id":"mix","level":0.2}]})
        );
        for (mutation, domain, method, params) in [
            (
                Mutation::Level(
                    WaveLinkTarget::Channel {
                        channel_id: "channel".into(),
                    },
                    0.2,
                ),
                Domain::Channels,
                "setChannel",
                json!({"id":"channel","level":0.2}),
            ),
            (
                Mutation::Level(
                    WaveLinkTarget::Mix {
                        mix_id: "mix".into(),
                    },
                    0.3,
                ),
                Domain::Mixes,
                "setMix",
                json!({"id":"mix","level":0.3}),
            ),
            (
                Mutation::Mute(
                    WaveLinkTarget::Input {
                        input_device_id: "micA".into(),
                        input_id: "same".into(),
                    },
                    true,
                ),
                Domain::Inputs,
                "setInputDevice",
                json!({"id":"micA","inputs":[{"id":"same","isMuted":true}]}),
            ),
            (
                Mutation::Mute(
                    WaveLinkTarget::Output {
                        output_device_id: "outB".into(),
                        output_id: "same".into(),
                    },
                    false,
                ),
                Domain::Outputs,
                "setOutputDevice",
                json!({"outputDevice":{"id":"outB","outputs":[{"id":"same","isMuted":false}]}}),
            ),
            (
                Mutation::Level(
                    WaveLinkTarget::Output {
                        output_device_id: "outA".into(),
                        output_id: "same".into(),
                    },
                    0.4,
                ),
                Domain::Outputs,
                "setOutputDevice",
                json!({"outputDevice":{"id":"outA","outputs":[{"id":"same","level":0.4}]}}),
            ),
        ] {
            assert_eq!(
                mutation_request(&state, &mutation),
                Ok((domain, method, params))
            );
        }
        let output = WaveLinkMainOutput {
            output_device_id: "outA".into(),
            output_id: "same".into(),
        };
        assert_eq!(
            mutation_request(&state, &Mutation::MainOutput(output.clone()))
                .unwrap()
                .2,
            json!({"mainOutput":{"outputDeviceId":"outA","outputId":"same"}})
        );
        assert_eq!(
            mutation_request(
                &state,
                &Mutation::MonitoredMix(output.clone(), "mix".into())
            )
            .unwrap()
            .2,
            json!({"outputDevice":{"id":"outA","outputs":[{"id":"same","mixId":"mix"}]}})
        );
        for mix in ["", "missing"] {
            assert!(
                mutation_request(&state, &Mutation::MonitoredMix(output.clone(), mix.into()))
                    .is_err()
            );
        }
        assert!(mutation_request(
            &state,
            &Mutation::MonitoredMix(
                WaveLinkMainOutput {
                    output_device_id: "outB".into(),
                    output_id: "same".into()
                },
                "mix".into()
            )
        )
        .is_err());
        for target in [
            WaveLinkTarget::InputGain {
                input_device_id: "micA".into(),
                input_id: "same".into(),
            },
            WaveLinkTarget::Input {
                input_device_id: "micB".into(),
                input_id: "same".into(),
            },
        ] {
            assert!(mutation_request(&state, &Mutation::Mute(target.clone(), true)).is_err());
            assert!(mutation_request(&state, &Mutation::Level(target, 0.5)).is_err());
        }
        state.status = WaveLinkStatus::Disconnected;
        assert!(mutation_request(&state, &Mutation::MainOutput(output)).is_err());
    }

    #[tokio::test]
    async fn command_queue_is_bounded_during_identification_and_shutdown_cancels_it() {
        let fixture = super::super::tests::Fixture::new().await;
        let wave = fixture.start(true);
        let mut peer = fixture.accept().await;
        assert_eq!(peer.request().await["method"], "getApplicationInfo");
        for _ in 0..PENDING_LIMIT {
            wave.inner.commands.try_send(Command::Refresh).unwrap();
        }
        assert!(matches!(
            wave.inner.commands.try_send(Command::Refresh),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        wave.shutdown();
        peer.closed().await;
    }

    #[tokio::test]
    async fn pending_limit_rejects_before_sending_and_does_not_grow() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        assert!(!(1884..=1893).contains(&address.port()));
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            tokio_tungstenite::MaybeTlsStream::Plain(stream),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut session = Session {
            catalog: Catalog::default(),
            unsupported: HashMap::new(),
            versions: HashMap::new(),
            pending: HashMap::new(),
            reads: HashMap::new(),
            next_id: 32,
        };
        for id in 0..PENDING_LIMIT as u64 {
            session.pending.insert(
                id,
                Pending {
                    purpose: Purpose::Read {
                        domain: Domain::Channels,
                        fence: 0,
                        retries: 0,
                        reply: None,
                    },
                    deadline: Instant::now() + RPC_TIMEOUT,
                },
            );
        }
        let result = session
            .send(
                &mut socket,
                "getChannels",
                None,
                Purpose::Read {
                    domain: Domain::Channels,
                    fence: 0,
                    retries: 0,
                    reply: None,
                },
            )
            .await;
        assert!(result.unwrap_err().contains("pending request limit"));
        assert_eq!(session.pending.len(), PENDING_LIMIT);
        assert_eq!(session.next_id, 32);
    }
}
