//! The remote against a real macro engine, and the server over loopback only.

use super::http::AssetSource;
use super::protocol::*;
use super::session::Session;
use super::*;
use crate::config::SettingsStore;
use crate::macros::{Action, ActionKind, EngineSink, RunningMacro, Services, VarScope};
use crate::midi::models::driver_for;
use crate::midi::render::{bump, frame_ticks, FrameTicks, RenderInputs};
use crate::midi::{
    claim_remote_press, remote_pressable, ButtonEvent, ConnectedDevice, LaunchpadModel, Layout,
};
use crate::profile::{
    Button, Fader, FaderDirection, Look, PadColor, Page, Profile, DEFAULT_PAGE_ID,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Repaints bump the ticks, as the app's sink does through the LED renderer.
struct TickSink(FrameTicks);
impl EngineSink for TickSink {
    fn running_changed(&self, _: &[RunningMacro]) {
        bump(&self.0);
    }
    fn profile_changed(&self, _: &Profile) {}
    fn repaint(&self) {
        bump(&self.0);
    }
}

/// The app backend's logic over a virtual Launchpad: the same claim, pressability,
/// fader and page paths, with the engine's input instead of the device manager.
struct TestBackend {
    engine: MacroEngine,
    profile: Arc<Mutex<ProfileStore>>,
    layout: Layout,
    inputs: RenderInputs,
    owners: Mutex<HashMap<(u8, u8), u64>>,
    device: ConnectedDevice,
}

impl RemoteBackend for TestBackend {
    fn state(&self) -> RemoteState {
        build_state_from(&self.device, &self.layout, &self.inputs)
    }
    fn press(&self, session: u64, x: u8, y: u8, pressed: bool) -> Result<bool, BackendError> {
        if pressed && !remote_pressable(&self.layout, x, y) {
            return Ok(false);
        }
        if !claim_remote_press(&self.inputs.pressed, &self.owners, session, x, y, pressed) {
            return Ok(false);
        }
        bump(&self.inputs.ticks);
        self.engine.on_button(&ButtonEvent {
            x,
            y,
            pressed,
            note: 0,
            cc: false,
            value: if pressed { 127 } else { 0 },
        });
        Ok(true)
    }
    fn set_fader(&self, fader_id: &str, fraction: f64) -> Result<(), BackendError> {
        let event = fader_control(&self.profile, fader_id, fraction)?;
        self.engine.on_control(&event);
        Ok(())
    }
    fn switch_page(&self, page_id: &str) -> Result<(), BackendError> {
        if self.engine.switch_page(page_id) {
            Ok(())
        } else {
            Err(BackendError::UnknownPage)
        }
    }
    fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inputs.ticks.subscribe()
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lunchpad-remote-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn backend() -> Arc<TestBackend> {
    let dir = temp_dir("profile");
    let profile = Arc::new(Mutex::new(ProfileStore::load(&dir)));
    let ticks = frame_ticks();
    let running = Arc::new(Mutex::new(HashSet::new()));
    let engine = MacroEngine::new(
        profile.clone(),
        running.clone(),
        Arc::new(TickSink(ticks.clone())),
        Services::default(),
        tokio::runtime::Handle::current(),
    );
    let model = LaunchpadModel::LaunchpadMk2;
    let inputs = RenderInputs {
        profile: profile.clone(),
        pressed: Arc::new(Mutex::new(HashSet::new())),
        running,
        press_feedback: Arc::new(Mutex::new(true)),
        live: Default::default(),
        ticks,
    };
    let device = ConnectedDevice {
        model,
        model_name: model.display_name().into(),
        input_name: String::new(),
        output_name: String::new(),
        firmware: None,
        is_virtual: true,
    };
    Arc::new(TestBackend {
        engine,
        profile,
        layout: driver_for(model).layout(),
        inputs,
        owners: Mutex::new(HashMap::new()),
        device,
    })
}

fn mark(letter: &str) -> Action {
    Action::new(ActionKind::SetVariable {
        name: "seq".into(),
        value: format!("{{{{seq}}}}{letter}"),
        scope: VarScope::Global,
    })
}

fn set_button(b: &TestBackend, x: u8, y: u8, button: Button) {
    b.profile
        .lock()
        .profile
        .page_mut(DEFAULT_PAGE_ID)
        .unwrap()
        .set(x, y, button);
}

/// A global once it reads `expected`, or what it reads when a generous deadline passes.
async fn seq_once(b: &TestBackend, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let value = b.engine.globals().get("seq").cloned().unwrap_or_default();
        if value == expected || Instant::now() >= deadline {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn press(x: u8, y: u8) -> String {
    format!(r#"{{"type":"press","x":{x},"y":{y}}}"#)
}
fn release(x: u8, y: u8) -> String {
    format!(r#"{{"type":"release","x":{x},"y":{y}}}"#)
}
fn error_code(reply: Option<ServerMessage>) -> Option<String> {
    match reply {
        Some(ServerMessage::Error { code, .. }) => Some(code),
        _ => None,
    }
}

fn hold_button() -> Button {
    Button {
        hold_wait: true,
        hold_ms: 120,
        down: vec![mark("D")],
        hold: vec![mark("H")],
        up: vec![mark("U")],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_remote_tap_and_hold_follow_the_hardware_semantics() {
    let b = backend();
    set_button(&b, 0, 0, hold_button());
    b.engine.set_global("seq", String::new()).unwrap();
    let mut s = Session::new(1, b.clone());

    // A tap: the pressed list runs on release, then the released list.
    assert!(s.handle(&press(0, 0)).is_none());
    assert!(
        b.inputs.pressed.lock().contains(&(0, 0)),
        "the pad lights as held"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(s.handle(&release(0, 0)).is_none());
    assert_eq!(seq_once(&b, "DU").await, "DU");
    assert!(b.inputs.pressed.lock().is_empty());

    // A hold: the held list after hold_ms, the pressed list never.
    b.engine.set_global("seq", String::new()).unwrap();
    s.handle(&press(0, 0));
    assert_eq!(seq_once(&b, "H").await, "H");
    s.handle(&release(0, 0));
    assert_eq!(seq_once(&b, "HU").await, "HU");
}

#[tokio::test]
async fn a_pad_has_one_gesture_whatever_holds_it() {
    let b = backend();
    set_button(
        &b,
        1,
        1,
        Button {
            down: vec![mark("D")],
            up: vec![mark("U")],
            ..Default::default()
        },
    );
    b.engine.set_global("seq", String::new()).unwrap();
    let mut phone = Session::new(1, b.clone());
    let mut other = Session::new(2, b.clone());

    assert!(phone.handle(&press(1, 1)).is_none());
    assert!(
        phone.handle(&press(1, 1)).is_none(),
        "a repeated press of a held pad is ignored"
    );
    assert_eq!(
        error_code(other.handle(&press(1, 1))).as_deref(),
        Some("busy"),
        "another phone cannot take the pad"
    );
    assert!(other.handle(&release(1, 1)).is_none());
    assert!(
        b.inputs.pressed.lock().contains(&(1, 1)),
        "a release from a phone that does not hold it does nothing"
    );
    phone.handle(&release(1, 1));
    assert_eq!(
        seq_once(&b, "DU").await,
        "DU",
        "exactly one down and one up"
    );

    // Held on the hardware (or the window): the phone is refused.
    b.inputs.pressed.lock().insert((2, 2));
    assert_eq!(
        error_code(phone.handle(&press(2, 2))).as_deref(),
        Some("busy")
    );
    // An LED-only spot or empty corner cannot be pressed.
    assert_eq!(
        error_code(phone.handle(&press(8, 8))).as_deref(),
        Some("busy")
    );
    assert_eq!(
        error_code(phone.handle(&press(40, 40))).as_deref(),
        Some("busy")
    );
}

#[test]
fn the_hardware_release_ends_a_remote_gesture_and_the_late_remote_release_is_ignored() {
    let pressed = Mutex::new(HashSet::new());
    let owners = Mutex::new(HashMap::new());
    assert!(claim_remote_press(&pressed, &owners, 7, 0, 0, true));
    // The hardware lets go (the input callback removes the pad and its owner)...
    pressed.lock().remove(&(0, 0));
    owners.lock().remove(&(0, 0));
    // ...and presses again: that gesture is the hardware's now.
    pressed.lock().insert((0, 0));
    assert!(
        !claim_remote_press(&pressed, &owners, 7, 0, 0, false),
        "the phone's late release does not end the hardware's press"
    );
    assert!(pressed.lock().contains(&(0, 0)));
}

#[tokio::test]
async fn closing_a_session_releases_everything_it_held() {
    let b = backend();
    set_button(&b, 0, 0, hold_button());
    set_button(
        &b,
        1,
        0,
        Button {
            down: vec![mark("d")],
            up: vec![mark("u")],
            ..Default::default()
        },
    );
    b.engine.set_global("seq", String::new()).unwrap();
    let mut s = Session::new(1, b.clone());
    s.handle(&press(0, 0));
    s.handle(&press(1, 0));
    assert_eq!(s.held().len(), 2);
    s.release_all();
    assert!(s.held().is_empty());
    assert!(b.inputs.pressed.lock().is_empty(), "no pad stays held");
    assert!(until(|| b.engine.globals().get("seq").map(|s| s.len()) == Some(4)).await);
    let seq = b.engine.globals().get("seq").cloned().unwrap_or_default();
    let mut letters: Vec<char> = seq.chars().collect();
    letters.sort();
    assert_eq!(
        letters,
        vec!['D', 'U', 'd', 'u'],
        "every press got its release ({seq})"
    );
}

fn add_fader(b: &TestBackend, page: &str) {
    let mut store = b.profile.lock();
    if store.profile.page(page).is_none() {
        store.profile.pages.push(Page::new(page, page));
    }
    store.profile.page_mut(page).unwrap().set_fader(Fader {
        id: format!("{page}-f"),
        name: "level".into(),
        x: 0,
        y: 0,
        direction: FaderDirection::Up,
        length: 5,
        min: 0.0,
        max: 100.0,
        on_change: vec![Action::new(ActionKind::SetVariable {
            name: "moved".into(),
            value: "{{value}}".into(),
            scope: VarScope::Global,
        })],
        ..Fader::default()
    });
}

#[tokio::test]
async fn a_fader_moves_like_a_knob_on_the_page_on_screen_only() {
    let b = backend();
    add_fader(&b, DEFAULT_PAGE_ID);
    add_fader(&b, "other");
    let mut s = Session::new(1, b.clone());
    assert!(s
        .handle(&format!(
            r#"{{"type":"setFader","faderId":"{DEFAULT_PAGE_ID}-f","value":0.5}}"#
        ))
        .is_none());
    let deadline = Instant::now() + Duration::from_secs(3);
    while b.engine.globals().get("moved").map(String::as_str) != Some("50")
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        b.engine.globals().get("moved").map(String::as_str),
        Some("50"),
        "the fader's actions ran with the value"
    );
    assert_eq!(
        b.profile
            .lock()
            .profile
            .page(DEFAULT_PAGE_ID)
            .unwrap()
            .faders[0]
            .value,
        50.0
    );
    assert_eq!(
        error_code(s.handle(r#"{"type":"setFader","faderId":"other-f","value":0.5}"#)).as_deref(),
        Some("unknownFader"),
        "not on screen"
    );
    assert_eq!(
        error_code(s.handle(&format!(
            r#"{{"type":"setFader","faderId":"{DEFAULT_PAGE_ID}-f","value":1.5}}"#
        )))
        .as_deref(),
        Some("badRequest")
    );
    let state = b.state();
    let fader = &state.faders[0];
    assert_eq!((fader.value, fader.fraction), (50.0, 0.5));
    let id = format!("{DEFAULT_PAGE_ID}-f");
    assert_eq!(
        state
            .pads
            .iter()
            .filter(|p| p.fader.as_deref() == Some(id.as_str()))
            .count(),
        5
    );
}

#[tokio::test]
async fn switching_pages_goes_through_the_engine_and_unknown_pages_are_refused() {
    let b = backend();
    b.profile
        .lock()
        .profile
        .pages
        .push(Page::new("p2", "Second"));
    let mut s = Session::new(1, b.clone());
    let mut ticks = b.changes();
    assert!(s.handle(r#"{"type":"switchPage","pageId":"p2"}"#).is_none());
    assert_eq!(b.profile.lock().profile.active_page, "p2");
    assert!(ticks.has_changed().unwrap(), "the change is announced");
    ticks.mark_unchanged();
    let state = b.state();
    assert_eq!(state.active_page.as_deref(), Some("p2"));
    assert_eq!(state.pages.len(), 2);
    assert_eq!(
        error_code(s.handle(r#"{"type":"switchPage","pageId":"nope"}"#)).as_deref(),
        Some("unknownPage")
    );
    assert_eq!(b.profile.lock().profile.active_page, "p2");
}

#[tokio::test]
async fn only_the_remote_commands_are_accepted() {
    let b = backend();
    let mut s = Session::new(1, b.clone());
    for bad in [
        r#"{"type":"setButton","x":0,"y":0}"#,
        r#"{"type":"runScript","code":"1"}"#,
        r#"{"type":"press","x":0,"y":0,"extra":1}"#,
        r#"{"type":"press","x":-1,"y":0}"#,
        "not json",
    ] {
        assert_eq!(
            error_code(s.handle(bad)).as_deref(),
            Some("badRequest"),
            "{bad}"
        );
    }
    assert_eq!(
        error_code(s.handle(r#"{"type":"auth","v":1,"token":"x"}"#)).as_deref(),
        Some("alreadyAuthenticated")
    );
    assert!(matches!(
        s.handle(r#"{"type":"ping"}"#),
        Some(ServerMessage::Pong)
    ));
}

#[tokio::test]
async fn the_state_shows_exactly_the_hardware_frame_with_labels() {
    let b = backend();
    set_button(
        &b,
        0,
        0,
        Button {
            color: PadColor::Palette { index: 5 },
            active_color: Some(PadColor::Palette { index: 21 }),
            look: Look::Text {
                caption: "Mic".into(),
                size: 16,
                face: "sans".into(),
                color: "#ffffff".into(),
            },
            ..Default::default()
        },
    );
    set_button(
        &b,
        1,
        0,
        Button {
            color: PadColor::Flashing { index: 5, alt: 9 },
            ..Default::default()
        },
    );
    set_button(
        &b,
        2,
        0,
        Button {
            color: PadColor::Rgb { r: 1, g: 2, b: 3 },
            look: Look::Image {
                uri: "/private/path.png".into(),
            },
            ..Default::default()
        },
    );
    add_fader(&b, DEFAULT_PAGE_ID);
    b.profile
        .lock()
        .profile
        .page_mut(DEFAULT_PAGE_ID)
        .unwrap()
        .faders[0]
        .x = 7;
    b.inputs.pressed.lock().insert((0, 0));

    let state = b.state();
    let frame = crate::midi::render::build_frame(&b.layout, &b.inputs);
    assert!(!state.pads.is_empty());
    for pad in &state.pads {
        let led = frame
            .get(&(pad.x, pad.y))
            .copied()
            .unwrap_or(crate::midi::LedColor::Off);
        assert_eq!(pad.color, RemoteColor::from(led), "pad {},{}", pad.x, pad.y);
    }
    let at = |x: u8, y: u8| state.pads.iter().find(|p| p.x == x && p.y == y).unwrap();
    assert_eq!(
        at(0, 0).color,
        RemoteColor::Palette {
            index: 21,
            rgb: "#00fc00".into()
        },
        "held: the active colour"
    );
    assert!(at(0, 0).pressed);
    assert_eq!(
        at(0, 0).label,
        Some(RemoteLabel::Text {
            caption: "Mic".into(),
            color: "#ffffff".into()
        })
    );
    assert_eq!(
        at(2, 0).label,
        Some(RemoteLabel::Image),
        "pictures are flagged, their paths are not sent"
    );
    assert!(!serde_json::to_string(&state)
        .unwrap()
        .contains("/private/path.png"));
    assert!(matches!(
        at(1, 0).color,
        RemoteColor::Flashing {
            index: 5,
            alt: 9,
            ..
        }
    ));
    assert_eq!(at(7, 2).fader, Some(format!("{DEFAULT_PAGE_ID}-f")));

    let json = serde_json::to_value(ServerMessage::State(Box::new(state))).unwrap();
    assert_eq!(json["type"], "state");
    assert_eq!(json["v"], 1);
    assert_eq!(json["device"]["virtual"], true);
    assert!(json["layout"]["pads"].is_array());
}

// ----- over loopback -------------------------------------------------------------

struct NoAssets;
impl AssetSource for NoAssets {
    fn get(&self, _: &str) -> Option<(Vec<u8>, String)> {
        None
    }
}

fn service(
    b: Arc<TestBackend>,
    limits: Limits,
) -> (RemoteService, SharedSettings, std::path::PathBuf) {
    let dir = temp_dir("settings");
    let settings: SharedSettings = Arc::new(Mutex::new(SettingsStore::load(&dir)));
    {
        let mut st = settings.lock();
        st.settings.remote.enabled = true;
        st.settings.remote.port = 0;
    }
    let options = Options {
        bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        addresses: Arc::new(|| vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        mdns: false,
        limits,
    };
    (
        RemoteService::new(settings.clone(), b, Arc::new(NoAssets), options),
        settings,
        dir,
    )
}

type Client = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn connect(
    port: u16,
    origin: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .map(|(ws, _)| ws)
}

async fn next_json(ws: &mut Client) -> Option<serde_json::Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .ok()??
        {
            Ok(Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}

async fn send_json(ws: &mut Client, value: serde_json::Value) {
    ws.send(Message::text(value.to_string())).await.unwrap();
}

/// Read until the server closes (or the timeout passes); whether it did.
async fn closed(ws: &mut Client) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            _ => {}
        }
    }
    false
}

fn port_of(service: &RemoteService) -> u16 {
    service.inner.running_port.lock().expect("listening")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_then_signing_in_with_the_stored_token() {
    let b = backend();
    let (service, settings, dir) = service(b.clone(), Limits::default());
    let status = service.apply_settings().await;
    assert!(status.running);
    let port = port_of(&service);
    assert_eq!(status.urls, vec![format!("http://127.0.0.1:{port}/")]);

    let pairing = service.start_pairing().unwrap();
    assert_eq!(pairing.code.len(), 8);
    assert_eq!(
        pairing.url,
        Some(format!("http://127.0.0.1:{port}/#pair={}", pairing.code)),
        "the code travels in the fragment"
    );

    let mut ws = connect(port, Some(&format!("http://127.0.0.1:{port}")))
        .await
        .unwrap();
    send_json(&mut ws, serde_json::json!({ "type": "pair", "v": 1, "code": pairing.code, "name": "  Pixel\u{7}  " })).await;
    let paired = next_json(&mut ws).await.unwrap();
    assert_eq!(paired["type"], "paired");
    assert_eq!(paired["name"], "Pixel");
    let token = paired["token"].as_str().unwrap().to_string();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "state");
    assert_eq!(service.status().connected.len(), 1);
    assert!(service.status().pairing.is_none(), "the code is used up");
    drop(ws);

    // Stored hashed, in the settings file, never in the clear.
    let file = std::fs::read_to_string(dir.join("settings.json")).unwrap();
    assert!(!file.contains(&token));
    let stored = settings.lock().settings.remote.devices.clone();
    assert_eq!(stored.len(), 1);
    assert!(file.contains(&stored[0].token_hash));
    assert_eq!(stored[0].token_hash, pairing::hash_token(&token));

    // The same code does not pair a second phone.
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "pair", "v": 1, "code": pairing.code }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["code"], "noPairing");
    assert!(closed(&mut ws).await);

    // Signing in with the token.
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "auth", "v": 1, "token": token }),
    )
    .await;
    let ok = next_json(&mut ws).await.unwrap();
    assert_eq!(ok["type"], "authOk");
    assert_eq!(ok["deviceId"], stored[0].id.as_str());
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "state");
    assert!(settings.lock().settings.remote.devices[0]
        .last_seen
        .is_some());
    service.configure(false, 4000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_and_cross_site_connections_are_refused() {
    let b = backend();
    set_button(
        &b,
        0,
        0,
        Button {
            down: vec![mark("D")],
            ..Default::default()
        },
    );
    let (service, _settings, _dir) = service(b.clone(), Limits::default());
    service.apply_settings().await;
    let port = port_of(&service);

    // A command before signing in.
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "press", "x": 0, "y": 0 }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["code"], "unauthorized");
    assert!(closed(&mut ws).await);
    // A made-up token.
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "auth", "v": 1, "token": pairing::new_token() }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["code"], "unauthorized");
    assert!(closed(&mut ws).await);
    // Another protocol version.
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "auth", "v": 99, "token": "x" }),
    )
    .await;
    assert_eq!(
        next_json(&mut ws).await.unwrap()["code"],
        "unsupportedVersion"
    );
    assert!(
        b.inputs.pressed.lock().is_empty() && !b.engine.globals().contains_key("seq"),
        "nothing ran"
    );

    // A web page elsewhere (DNS rebinding or cross-site) cannot open the socket.
    let err = connect(port, Some("http://evil.example"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 403),
        "{err:?}"
    );
    let raw = raw_get(port, "/ws", "evil.example:80").await;
    assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");
    let raw = raw_get(port, "/", "evil.example").await;
    assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");
    let raw = raw_get(port, "/", &format!("127.0.0.1:{port}")).await;
    assert!(
        raw.starts_with("HTTP/1.1 200") && raw.contains("Lunchpad remote"),
        "{raw}"
    );
    let raw = raw_get(port, "/index.html", &format!("127.0.0.1:{port}")).await;
    assert!(raw.starts_with("HTTP/1.1 404"), "{raw}");
    service.configure(false, 4000).await.unwrap();
}

async fn raw_get(port: u16, path: &str, host: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut out)).await;
    out
}

/// Pair over loopback and return the token.
async fn paired_token(service: &RemoteService, port: u16) -> String {
    let code = service.start_pairing().unwrap().code;
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "pair", "v": 1, "code": code, "name": "Phone" }),
    )
    .await;
    let token = next_json(&mut ws).await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = ws.close(None).await;
    token
}

async fn signed_in(port: u16, token: &str) -> Client {
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "auth", "v": 1, "token": token }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "authOk");
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "state");
    ws
}

async fn until(what: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !what() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    what()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_socket_releases_its_holds_and_live_changes_are_pushed() {
    let b = backend();
    set_button(&b, 0, 0, hold_button());
    b.engine.set_global("seq", String::new()).unwrap();
    let (service, _settings, _dir) = service(b.clone(), Limits::default());
    service.apply_settings().await;
    let port = port_of(&service);
    let token = paired_token(&service, port).await;
    let mut ws = signed_in(port, &token).await;

    send_json(
        &mut ws,
        serde_json::json!({ "type": "press", "x": 0, "y": 0 }),
    )
    .await;
    // The press shows up as a pushed state with the pad held.
    let mut held = false;
    for _ in 0..5 {
        let state = next_json(&mut ws).await.unwrap();
        if state["pads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["x"] == 0 && p["y"] == 0 && p["pressed"] == true)
        {
            held = true;
            break;
        }
    }
    assert!(held, "the held pad was pushed");
    assert_eq!(seq_once(&b, "H").await, "H");

    // The phone vanishes without a goodbye.
    drop(ws);
    assert!(
        until(|| b.inputs.pressed.lock().is_empty()).await,
        "the hold was released"
    );
    assert_eq!(seq_once(&b, "HU").await, "HU");
    assert!(until(|| service.status().connected.is_empty()).await);
    service.configure(false, 4000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_phone_times_out_and_lets_go() {
    let b = backend();
    set_button(
        &b,
        3,
        3,
        Button {
            down: vec![mark("D")],
            up: vec![mark("U")],
            ..Default::default()
        },
    );
    b.engine.set_global("seq", String::new()).unwrap();
    let limits = Limits {
        idle_timeout: Duration::from_millis(300),
        ping_interval: Duration::from_millis(100),
        ..Limits::default()
    };
    let (service, _settings, _dir) = service(b.clone(), limits);
    service.apply_settings().await;
    let port = port_of(&service);
    let token = paired_token(&service, port).await;
    let mut ws = signed_in(port, &token).await;
    send_json(
        &mut ws,
        serde_json::json!({ "type": "press", "x": 3, "y": 3 }),
    )
    .await;
    assert!(until(|| b.inputs.pressed.lock().contains(&(3, 3))).await);
    // Not reading: our pongs never go out, the server hears nothing.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        until(|| b.inputs.pressed.lock().is_empty()).await,
        "released after the timeout"
    );
    assert_eq!(seq_once(&b, "DU").await, "DU");
    service.configure(false, 4000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_a_phone_disconnects_it_and_its_token_stops_working() {
    let b = backend();
    let (service, settings, _dir) = service(b.clone(), Limits::default());
    service.apply_settings().await;
    let port = port_of(&service);
    let token = paired_token(&service, port).await;
    let mut ws = signed_in(port, &token).await;
    let id = settings.lock().settings.remote.devices[0].id.clone();
    assert!(
        service.status().devices[0].connected
            || until(|| service.status().devices[0].connected).await
    );

    let status = service.revoke(&id).unwrap();
    assert!(status.devices.is_empty());
    assert!(closed(&mut ws).await, "the live socket is cut off");
    let mut ws = connect(port, None).await.unwrap();
    send_json(
        &mut ws,
        serde_json::json!({ "type": "auth", "v": 1, "token": token }),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["code"], "unauthorized");
    service.configure(false, 4000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_and_flooding_messages_end_the_connection() {
    let b = backend();
    let limits = Limits {
        max_messages_per_second: 20,
        ..Limits::default()
    };
    let (service, _settings, _dir) = service(b.clone(), limits);
    service.apply_settings().await;
    let port = port_of(&service);
    let token = paired_token(&service, port).await;

    let mut ws = signed_in(port, &token).await;
    let _ = ws
        .send(Message::text(format!(
            r#"{{"type":"ping","pad":"{}"}}"#,
            "x".repeat(10_000)
        )))
        .await;
    assert!(
        closed(&mut ws).await,
        "a message over the limit closes the socket"
    );

    let mut ws = signed_in(port, &token).await;
    for _ in 0..40 {
        if ws.send(Message::text(r#"{"type":"ping"}"#)).await.is_err() {
            break;
        }
    }
    let mut flood = false;
    while let Some(v) = next_json(&mut ws).await {
        if v["code"] == "tooManyMessages" {
            flood = true;
            break;
        }
    }
    assert!(flood);
    service.configure(false, 4000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_it_off_stops_listening_and_releases_holds() {
    let b = backend();
    set_button(
        &b,
        0,
        0,
        Button {
            down: vec![mark("D")],
            up: vec![mark("U")],
            ..Default::default()
        },
    );
    b.engine.set_global("seq", String::new()).unwrap();
    let (service, settings, _dir) = service(b.clone(), Limits::default());
    assert!(!service.status().running);
    service.apply_settings().await;
    let port = port_of(&service);
    let token = paired_token(&service, port).await;
    let mut ws = signed_in(port, &token).await;
    send_json(
        &mut ws,
        serde_json::json!({ "type": "press", "x": 0, "y": 0 }),
    )
    .await;
    assert!(until(|| b.inputs.pressed.lock().contains(&(0, 0))).await);
    service.start_pairing().unwrap();

    settings.lock().settings.remote.enabled = false;
    let status = service.apply_settings().await;
    assert!(!status.running && status.urls.is_empty() && status.pairing.is_none());
    assert!(
        b.inputs.pressed.lock().is_empty(),
        "stopping waited for the holds to be released"
    );
    assert_eq!(seq_once(&b, "DU").await, "DU");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "nothing listens any more"
    );
    assert!(service.start_pairing().is_err(), "no pairing while off");
    assert_eq!(
        settings.lock().settings.remote.devices.len(),
        1,
        "turning it off forgets no phone"
    );

    assert!(
        service.configure(true, 80).await.is_err(),
        "privileged ports are refused"
    );
    let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    {
        let mut st = settings.lock();
        st.settings.remote.enabled = true;
        st.settings.remote.port = taken.local_addr().unwrap().port();
    }
    let status = service.apply_settings().await;
    assert!(
        !status.running && status.error.is_some(),
        "a taken port is reported, not hidden"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_codes_are_rate_limited_over_the_wire() {
    let b = backend();
    let (service, _settings, _dir) = service(b, Limits::default());
    service.apply_settings().await;
    let port = port_of(&service);
    let code = service.start_pairing().unwrap().code;
    let wrong = if code == "00000000" {
        "11111111"
    } else {
        "00000000"
    };
    let mut codes = Vec::new();
    for _ in 0..pairing::MAX_FAILURES_PER_ADDRESS + 1 {
        let mut ws = connect(port, None).await.unwrap();
        send_json(
            &mut ws,
            serde_json::json!({ "type": "pair", "v": 1, "code": wrong }),
        )
        .await;
        codes.push(
            next_json(&mut ws).await.unwrap()["code"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(codes[..5], ["unauthorized"; 5], "five wrong guesses...");
    assert_eq!(codes[5], "noPairing", "...burn the code");
    assert_eq!(
        codes.last().map(String::as_str),
        Some("rateLimited"),
        "and lock the address out"
    );
    service.configure(false, 4000).await.unwrap();
}
