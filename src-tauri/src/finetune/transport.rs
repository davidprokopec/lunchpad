use super::*;
use crate::config::FineTuneSettings;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const PENDING_LIMIT: usize = 32;
pub(super) const FRAME_LIMIT: usize = 1_048_576;
type Reply = crossbeam_channel::Sender<Result<(), String>>;

#[derive(Clone)]
struct Desired {
    config: FineTuneSettings,
    connect: bool,
    shutdown: bool,
    epoch: u64,
}

struct Inner {
    state: Mutex<FineTuneState>,
    changes: watch::Sender<FineTuneState>,
    desired: watch::Sender<Desired>,
    epoch: AtomicU64,
    commands: mpsc::Sender<Command>,
}

#[derive(Clone)]
pub struct FineTune {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone)]
pub(crate) enum Mutation {
    Volume(String, f64),
    Mute(String, bool),
    Routing(String, FineTuneRoutingRequest),
    DefaultOutput(String),
    PreferredInput(String),
}

enum Command {
    Write {
        generation: u64,
        mutation: Mutation,
        reply: Reply,
    },
    Refresh,
    #[cfg(test)]
    Barrier(tokio::sync::oneshot::Sender<()>),
}

impl FineTune {
    pub fn start(config: FineTuneSettings) -> Self {
        // Test builds must never discover or connect to an installed FineTune.
        #[cfg(all(not(test), target_os = "macos"))]
        let path = Some(
            PathBuf::from(objc2_foundation::NSHomeDirectory().to_string())
                .join("Library/Application Support/FineTune/Control/control.sock"),
        );
        #[cfg(any(test, not(target_os = "macos")))]
        let path = None;
        Self::start_at(config, path)
    }

    pub(super) fn start_at(config: FineTuneSettings, path: Option<PathBuf>) -> Self {
        #[cfg(test)]
        if let Some(path) = &path {
            assert!(path.starts_with(std::env::temp_dir()));
            assert!(!path.ends_with("FineTune/Control/control.sock"));
        }
        let (commands, receiver) = mpsc::channel(PENDING_LIMIT);
        let (desired, control) = watch::channel(Desired {
            connect: config.enabled && config.auto_connect,
            config,
            shutdown: false,
            epoch: 0,
        });
        let (changes, _) = watch::channel(FineTuneState::default());
        let inner = Arc::new(Inner {
            state: Mutex::new(FineTuneState::default()),
            changes,
            desired,
            epoch: AtomicU64::new(0),
            commands,
        });
        let worker = inner.clone();
        tauri::async_runtime::spawn(supervise(worker, control, receiver, path));
        Self { inner }
    }

    pub fn state(&self) -> FineTuneState {
        self.inner.state.lock().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<FineTuneState> {
        self.inner.changes.subscribe()
    }
    pub fn configure(&self, config: FineTuneSettings) {
        self.change(Some(config), false, false);
    }
    pub fn connect(&self) -> FineTuneState {
        if !self.inner.desired.borrow().connect
            || matches!(
                self.state().status,
                FineTuneStatus::Disconnected | FineTuneStatus::Unsupported | FineTuneStatus::Error
            )
        {
            self.change(None, true, false);
        }
        self.state()
    }
    pub fn disconnect(&self) -> FineTuneState {
        self.change(None, false, false);
        self.state()
    }
    pub fn shutdown(&self) {
        self.change(None, false, true);
    }
    pub fn refresh(&self) -> FineTuneState {
        let _ = self.inner.commands.try_send(Command::Refresh);
        self.state()
    }

    #[cfg(test)]
    pub(super) async fn barrier(&self) {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.inner
            .commands
            .send(Command::Barrier(reply))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
    }

    fn change(&self, config: Option<FineTuneSettings>, connect: bool, shutdown: bool) {
        self.inner.desired.send_if_modified(|desired| {
            if desired.shutdown {
                return false;
            }
            if let Some(config) = config {
                if config == desired.config {
                    return false;
                }
                desired.connect = config.enabled
                    && config.auto_connect
                    && (!desired.config.enabled || desired.connect);
                desired.config = config;
            } else {
                desired.connect = connect && desired.config.enabled;
            }
            desired.shutdown = shutdown;
            desired.epoch = self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let mut state = self.inner.state.lock();
            *state = FineTuneState {
                status: if shutdown || !desired.config.enabled {
                    FineTuneStatus::Disabled
                } else if !cfg!(target_os = "macos") {
                    FineTuneStatus::Unsupported
                } else if desired.connect {
                    FineTuneStatus::Connecting
                } else {
                    FineTuneStatus::Disconnected
                },
                revision: state.revision + 1,
                generation: state.generation + 1,
                ..Default::default()
            };
            self.inner.changes.send_replace(state.clone());
            true
        });
    }

    pub(crate) fn write(&self, generation: u64, mutation: Mutation) -> Result<(), String> {
        let state = self.state();
        if state.status != FineTuneStatus::Ready || state.generation != generation {
            return Err("FineTune is unavailable or the captured session ended".into());
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.inner
            .commands
            .try_send(Command::Write {
                generation,
                mutation,
                reply,
            })
            .map_err(|_| "FineTune request queue is full or stopped".to_string())?;
        response
            .recv_timeout(REQUEST_TIMEOUT * 2 + Duration::from_secs(1))
            .map_err(|_| "FineTune command timed out; result unconfirmed".to_string())?
    }
}

impl Inner {
    fn publish(&self, epoch: u64, update: impl FnOnce(&mut FineTuneState)) {
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

async fn supervise(
    inner: Arc<Inner>,
    mut control: watch::Receiver<Desired>,
    mut commands: mpsc::Receiver<Command>,
    path: Option<PathBuf>,
) {
    loop {
        let desired = control.borrow_and_update().clone();
        if desired.shutdown {
            break;
        }
        if !desired.config.enabled || !desired.connect || !cfg!(target_os = "macos") {
            inner.publish(desired.epoch, |state| {
                if !desired.config.enabled {
                    state.status = FineTuneStatus::Disabled;
                } else if !cfg!(target_os = "macos") {
                    state.status = FineTuneStatus::Unsupported;
                } else if state.status != FineTuneStatus::Unsupported {
                    state.status = FineTuneStatus::Disconnected;
                }
            });
            tokio::select! { biased; result = control.changed() => if result.is_err() {break;}, command = commands.recv() => reject(command) }
            continue;
        }
        let mut delay = Duration::from_millis(250);
        loop {
            inner.publish(desired.epoch, |state| {
                state.status = FineTuneStatus::Connecting;
                state.error = None;
            });
            let outcome = tokio::select! { biased; _ = control.changed() => break, result = session(&inner, desired.epoch, &mut commands, path.as_deref()) => result };
            if inner.state.lock().status == FineTuneStatus::Ready {
                delay = Duration::from_millis(250);
            }
            inner.publish(desired.epoch, |state| {
                state.connected = false;
                state.snapshot = None;
                state.server_revision = None;
                state.session_id = None;
                state.app_version = None;
                state.status = if outcome.0 {
                    FineTuneStatus::Unsupported
                } else {
                    FineTuneStatus::Disconnected
                };
                state.error = Some(outcome.1);
            });
            // Anything admitted on the ended connection is rejected, never replayed.
            while let Ok(command) = commands.try_recv() {
                reject(Some(command));
            }
            if !desired.config.auto_connect || outcome.0 {
                inner.desired.send_if_modified(|current| {
                    if current.epoch != desired.epoch {
                        return false;
                    }
                    current.connect = false;
                    true
                });
                break;
            }
            let deadline = tokio::time::Instant::now() + delay;
            let changed = loop {
                tokio::select! { biased; _ = control.changed() => break true, _ = tokio::time::sleep_until(deadline) => break false, command = commands.recv() => reject(command) }
            };
            if changed {
                break;
            }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
    while let Ok(command) = commands.try_recv() {
        reject(Some(command));
    }
}

fn reject(command: Option<Command>) {
    match command {
        Some(Command::Write { reply, .. }) => {
            let _ = reply.send(Err("FineTune session ended; command not replayed".into()));
        }
        #[cfg(test)]
        Some(Command::Barrier(reply)) => {
            let _ = reply.send(());
        }
        _ => {}
    }
}

#[cfg(all(test, not(target_os = "macos")))]
#[tokio::test]
async fn non_macos_configuration_never_enables_finetune() {
    let client = FineTune::start_at(
        FineTuneSettings {
            enabled: true,
            auto_connect: true,
        },
        Some(std::env::temp_dir().join("unsupported-finetune.sock")),
    );
    client.barrier().await;
    assert_eq!(client.state().status, FineTuneStatus::Unsupported);
    for auto_connect in [false, true] {
        client.configure(FineTuneSettings {
            enabled: true,
            auto_connect,
        });
        assert_eq!(client.connect().status, FineTuneStatus::Unsupported);
        client.refresh();
        client.barrier().await;
        let state = client.state();
        assert_eq!(state.status, FineTuneStatus::Unsupported);
        assert!(!state.connected);
        assert!(state.snapshot.is_none());
    }
    client.configure(FineTuneSettings::default());
    client.barrier().await;
    assert_eq!(client.state().status, FineTuneStatus::Disabled);
    client.shutdown();
}

#[cfg(not(target_os = "macos"))]
async fn session(
    _: &Inner,
    _: u64,
    _: &mut mpsc::Receiver<Command>,
    _: Option<&std::path::Path>,
) -> (bool, String) {
    (
        true,
        "FineTune local control is supported only on macOS".into(),
    )
}

#[cfg(unix)]
pub(super) struct Frames<R> {
    pub(super) reader: tokio::io::BufReader<R>,
    payload: Vec<u8>,
    partial_since: Option<tokio::time::Instant>,
}

#[cfg(unix)]
impl<R: tokio::io::AsyncRead + Unpin> Frames<R> {
    pub(super) fn new(reader: R) -> Self {
        Self {
            reader: tokio::io::BufReader::new(reader),
            payload: Vec::new(),
            partial_since: None,
        }
    }
    pub(super) async fn next(&mut self) -> Result<Value, String> {
        use tokio::io::AsyncBufReadExt;
        loop {
            let fill = self.reader.fill_buf();
            let chunk = if let Some(start) = self.partial_since {
                tokio::time::timeout_at(start + Duration::from_secs(10), fill)
                    .await
                    .map_err(|_| "FineTune partial frame timeout")?
            } else {
                fill.await
            }
            .map_err(|e| e.to_string())?;
            if chunk.is_empty() {
                return Err("FineTune disconnected".into());
            }
            let end = chunk.iter().position(|b| *b == b'\n');
            let count = end.unwrap_or(chunk.len());
            if self.payload.len() + count > FRAME_LIMIT {
                return Err("FineTune frame too large".into());
            }
            self.payload.extend_from_slice(&chunk[..count]);
            self.reader.consume(count + usize::from(end.is_some()));
            if end.is_some() {
                self.partial_since = None;
                if self.payload.last() == Some(&b'\r') {
                    self.payload.pop();
                }
                let parsed =
                    serde_json::from_slice::<Value>(&self.payload).map_err(|e| e.to_string());
                self.payload.clear();
                return parsed.and_then(|value| {
                    if value.is_object() {
                        Ok(value)
                    } else {
                        Err("FineTune frame is not an object".into())
                    }
                });
            }
            self.partial_since
                .get_or_insert_with(tokio::time::Instant::now);
        }
    }
}

#[cfg(unix)]
struct Pending {
    reply: Option<Reply>,
    snapshot: bool,
    deadline: tokio::time::Instant,
}

#[cfg(unix)]
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err("FineTune request ended without confirmation".into()));
        }
    }
}

#[cfg(target_os = "macos")]
async fn session(
    inner: &Inner,
    epoch: u64,
    commands: &mut mpsc::Receiver<Command>,
    path: Option<&std::path::Path>,
) -> (bool, String) {
    match run_session(inner, epoch, commands, path).await {
        Ok(()) => (false, "FineTune disconnected".into()),
        Err(error) => error,
    }
}

#[cfg(target_os = "macos")]
async fn run_session(
    inner: &Inner,
    epoch: u64,
    commands: &mut mpsc::Receiver<Command>,
    path: Option<&std::path::Path>,
) -> Result<(), (bool, String)> {
    let failure = |message: String| (false, message);
    let path = path.ok_or_else(|| failure("FineTune socket path unavailable".into()))?;
    let socket = tokio::time::timeout(REQUEST_TIMEOUT, tokio::net::UnixStream::connect(path))
        .await
        .map_err(|_| failure("FineTune connection timed out".into()))?
        .map_err(|e| failure(e.to_string()))?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
            fn geteuid() -> u32;
        }
        let (mut uid, mut gid) = (0, 0);
        // The peer must be the same user before any protocol bytes are read.
        if unsafe { getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) != 0 || uid != geteuid() } {
            return Err(failure("FineTune peer identity check failed".into()));
        }
    }
    let (reader, mut writer) = socket.into_split();
    let mut frames = Frames::new(reader);
    let hello = tokio::time::timeout(REQUEST_TIMEOUT, frames.next())
        .await
        .map_err(|_| failure("FineTune hello timed out".into()))?
        .map_err(failure)?;
    if hello["type"] != "hello"
        || hello["protocol"] != "finetune-control"
        || hello["version"].as_u64() != Some(1)
    {
        return Err((true, "Unsupported FineTune control protocol/version".into()));
    }
    let session = hello["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| failure("Invalid FineTune sessionId".into()))?
        .to_owned();
    let app_version = hello["appVersion"]
        .as_str()
        .ok_or_else(|| failure("Missing FineTune appVersion".into()))?
        .to_owned();
    let capabilities = hello["capabilities"]
        .as_array()
        .ok_or_else(|| failure("Missing FineTune capabilities".into()))?;
    for method in [
        "getState",
        "subscribe",
        "setAppVolume",
        "setAppMute",
        "setAppRouting",
        "setDefaultOutput",
        "setPreferredInput",
    ] {
        if !capabilities.iter().any(|v| v == method) {
            return Err((true, format!("FineTune capability missing: {method}")));
        }
    }
    inner.publish(epoch, |state| {
        state.status = FineTuneStatus::Synchronizing;
        state.connected = true;
        state.session_id = Some(session.clone());
        state.app_version = Some(app_version);
        state.generation += 1;
        state.server_revision = None;
        state.snapshot = None;
    });
    let generation = inner.state.lock().generation;
    let mut next_id = 1u64;
    let mut pending = HashMap::<String, Pending>::new();
    let subscribe = request(&mut next_id, &session, "subscribe", json!({}));
    send(&mut writer, &subscribe).await.map_err(failure)?;
    pending.insert(
        subscribe["id"].as_str().unwrap().to_owned(),
        Pending {
            reply: None,
            snapshot: true,
            deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT,
        },
    );
    let mut timer = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! { biased;
            frame = frames.next() => {
                let frame = frame.map_err(failure)?;
                match frame["type"].as_str() {
                    Some("response") => {
                        let id = frame["id"].as_str().ok_or_else(|| failure("Invalid FineTune response id".into()))?;
                        if let Some(mut entry) = pending.remove(id) {
                            if tokio::time::Instant::now() >= entry.deadline { return Err(failure("FineTune request timed out; result unconfirmed".into())); }
                            let result = if let Some(error) = frame.get("error") {
                                let error: FineTuneError = serde_json::from_value(error.clone()).map_err(|e| failure(e.to_string()))?;
                                Err(format!("{}: {}", error.code, error.message))
                            } else if entry.snapshot {
                                apply_snapshot(inner, epoch, &session, &frame["result"]).map_err(failure)?;
                                Ok(())
                            } else if frame["result"]["accepted"] == true {
                                // Accepted is not state. Flush a full authoritative snapshot before
                                // releasing the shared audio FIFO (not an optimistic local write).
                                let readback = request(&mut next_id, &session, "getState", json!({}));
                                let id = readback["id"].as_str().unwrap().to_owned();
                                pending.insert(id, Pending { reply: entry.reply.take(), snapshot: true, deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT });
                                send(&mut writer, &readback).await.map_err(failure)?;
                                continue;
                            }
                            else { return Err(failure("Invalid FineTune command response".into())); };
                            if let Some(reply) = entry.reply.take() { let _ = reply.send(result); }
                            else if let Err(error) = result { return Err(failure(error)); }
                        }
                    }
                    Some("event") if frame["event"] == "stateChanged" => {
                        if inner.state.lock().status != FineTuneStatus::Ready { return Err(failure("FineTune event before subscribe response".into())); }
                        apply_snapshot(inner, epoch, &session, &frame).map_err(failure)?;
                    }
                    _ => return Err(failure("Invalid FineTune frame envelope".into())),
                }
            }
            _ = timer.tick() => {
                if pending.values().any(|p| tokio::time::Instant::now() >= p.deadline) { return Err(failure("FineTune request timed out; result unconfirmed".into())); }
            }
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                let (method, params, reply, snapshot) = match command {
                    #[cfg(test)]
                    Command::Barrier(reply) => { let _ = reply.send(()); continue; }
                    Command::Refresh => {
                        if pending.values().any(|p| p.snapshot) { continue; }
                        ("getState", json!({}), None, true)
                    }
                    Command::Write { generation: expected, mutation, reply } => {
                        let state = inner.state.lock().clone();
                        let params = if state.status != FineTuneStatus::Ready || generation != expected || inner.epoch.load(Ordering::Acquire) != epoch {
                            Err("FineTune captured session ended or is not ready".into())
                        } else { mutation_params(&state, mutation) };
                        match params {
                            Ok((method, params)) => (method, params, Some(reply), false),
                            Err(error) => { let _ = reply.send(Err(error)); continue; }
                        }
                    }
                };
                if pending.len() >= PENDING_LIMIT { if let Some(reply) = reply { let _ = reply.send(Err("FineTune pending request limit reached".into())); } continue; }
                let request = request(&mut next_id, &session, method, params);
                let id = request["id"].as_str().unwrap().to_owned();
                // Insert before awaiting I/O so cancellation also rejects this request.
                pending.insert(id, Pending { reply, snapshot, deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT });
                send(&mut writer, &request).await.map_err(failure)?;
            }
        }
    }
}

#[cfg(unix)]
async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, frame: &Value) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
    if bytes.len() > FRAME_LIMIT {
        return Err("FineTune request too large".into());
    }
    bytes.push(b'\n');
    tokio::time::timeout(REQUEST_TIMEOUT, writer.write_all(&bytes))
        .await
        .map_err(|_| "FineTune write timed out; result unconfirmed".to_string())?
        .map_err(|e| e.to_string())
}

fn request(next: &mut u64, session: &str, method: &str, params: Value) -> Value {
    let id = next.to_string();
    *next += 1;
    json!({"type":"request", "id":id, "sessionId":session, "method":method, "params":params})
}

fn apply_snapshot(inner: &Inner, epoch: u64, session: &str, frame: &Value) -> Result<(), String> {
    if frame["sessionId"].as_str() != Some(session) {
        return Err("FineTune snapshot session mismatch".into());
    }
    let revision = frame["revision"]
        .as_u64()
        .filter(|r| *r <= 9_007_199_254_740_991)
        .ok_or("Invalid FineTune revision")?;
    let snapshot: FineTuneSnapshot =
        serde_json::from_value(frame["state"].clone()).map_err(|e| e.to_string())?;
    for app in &snapshot.apps {
        if app.app_id.is_empty()
            || app.pids.len() != 1
            || app.pids[0] == 0
            || !app.volume.is_finite()
            || !(0.0..=1.0).contains(&app.volume)
            || !app.boost.is_finite()
        {
            return Err("Invalid FineTune app state".into());
        }
    }
    inner.publish(epoch, |state| {
        if state
            .server_revision
            .is_some_and(|current| revision <= current)
        {
            return;
        }
        state.snapshot = Some(snapshot);
        state.server_revision = Some(revision);
        state.status = FineTuneStatus::Ready;
    });
    Ok(())
}

fn mutation_params(
    state: &FineTuneState,
    mutation: Mutation,
) -> Result<(&'static str, Value), String> {
    use crate::profile::AudioDeviceKind;
    let snapshot = state
        .snapshot
        .as_ref()
        .ok_or("FineTune snapshot unavailable")?;
    let device = |uid: &str, direction| {
        if uid.is_empty()
            || !snapshot
                .devices
                .iter()
                .any(|d| d.uid == uid && d.direction == direction && d.capabilities.selectable)
        {
            Err("FineTune device is missing or not selectable".to_string())
        } else {
            Ok(())
        }
    };
    match mutation {
        Mutation::DefaultOutput(uid) => {
            device(&uid, AudioDeviceKind::Output)?;
            Ok(("setDefaultOutput", json!({"deviceUID":uid})))
        }
        Mutation::PreferredInput(uid) => {
            device(&uid, AudioDeviceKind::Input)?;
            Ok(("setPreferredInput", json!({"deviceUID":uid})))
        }
        mutation => {
            let app_id = match &mutation {
                Mutation::Volume(id, _) | Mutation::Mute(id, _) | Mutation::Routing(id, _) => id,
                _ => unreachable!(),
            };
            let app = state.app(app_id)?;
            if snapshot.permission.audio_capture != FineTuneCapturePermission::Authorized {
                return Err("FineTune audio capture permission is not authorized".into());
            }
            let mut params = json!({"appId":app_id, "pid":app.pids[0]});
            let method = match mutation {
                Mutation::Volume(_, volume) => {
                    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                        return Err("FineTune volume must be between 0 and 1".into());
                    }
                    params["volume"] = json!(volume);
                    "setAppVolume"
                }
                Mutation::Mute(_, muted) => {
                    params["muted"] = json!(muted);
                    "setAppMute"
                }
                Mutation::Routing(_, routing) => {
                    if matches!(app.routing.requested, FineTuneRouting::Multi { .. }) {
                        return Err("FineTune multi routing is read-only".into());
                    }
                    if let FineTuneRoutingRequest::Single { device_uid } = &routing {
                        device(device_uid, AudioDeviceKind::Output)?;
                    }
                    params["routing"] = serde_json::to_value(routing).map_err(|e| e.to_string())?;
                    "setAppRouting"
                }
                _ => unreachable!(),
            };
            Ok((method, params))
        }
    }
}
