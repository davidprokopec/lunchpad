use super::catalog::{Catalog, Domain};
use super::*;
use crate::config::WaveLinkSettings;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::Message;

struct CheckOrigin;
impl Callback for CheckOrigin {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        assert_eq!(request.headers().get("Origin").unwrap(), "streamdeck://");
        assert_eq!(request.uri().path(), "/");
        Ok(response)
    }
}

struct RejectOrigin;
impl Callback for RejectOrigin {
    fn on_request(self, request: &Request, _response: Response) -> Result<Response, ErrorResponse> {
        assert_eq!(request.headers().get("Origin").unwrap(), "streamdeck://");
        let mut response = ErrorResponse::new(Some("fixture rejects Origin".into()));
        *response.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
        Err(response)
    }
}

pub(crate) struct Fixture {
    listener: TcpListener,
    pub port: u16,
}
pub(crate) struct Peer {
    socket: tokio_tungstenite::WebSocketStream<TcpStream>,
}

impl Fixture {
    pub async fn new() -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!(1884..=1893).contains(&port));
        Self { listener, port }
    }
    pub fn start(&self, auto_connect: bool) -> WaveLink {
        WaveLink::start_with_ports(
            WaveLinkSettings {
                enabled: true,
                auto_connect,
                port: Some(self.port),
            },
            Some(vec![self.port]),
        )
    }
    pub async fn accept(&self) -> Peer {
        let (socket, address) = timeout(Duration::from_secs(5), self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(address.ip().is_loopback());
        let socket = tokio_tungstenite::accept_hdr_async(socket, CheckOrigin)
            .await
            .unwrap();
        Peer { socket }
    }
    pub async fn ready(&self, wave: &WaveLink) -> Peer {
        let mut peer = self.accept().await;
        let identify = peer.request().await;
        assert_eq!(identify["method"], "getApplicationInfo");
        peer.reply(
            &identify,
            json!({"name":"fixture","operatingSystem":"mac","appID":"EWL","interfaceRevision":2}),
        )
        .await;
        let mut requests = Vec::new();
        for _ in 0..4 {
            requests.push(peer.request().await);
        }
        for request in requests.into_iter().rev() {
            let result = catalog(request["method"].as_str().unwrap());
            peer.reply(&request, result).await;
        }
        until(wave, |state| state.status == WaveLinkStatus::Ready).await;
        peer
    }
}
impl Peer {
    pub async fn request(&mut self) -> Value {
        loop {
            let message = timeout(Duration::from_secs(5), self.socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_eq!(value["jsonrpc"], "2.0");
                assert!(value["id"].is_u64());
                return value;
            }
        }
    }
    pub async fn send(&mut self, value: Value) {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    pub async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            .await;
    }
    pub async fn notification(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await;
    }
    pub async fn getter(&mut self) -> String {
        let request = self.request().await;
        let method = request["method"].as_str().unwrap().to_string();
        assert!(
            method.starts_with("get"),
            "feedback must never mutate: {request}"
        );
        self.reply(&request, catalog(&method)).await;
        method
    }
    pub async fn mutation_readback(&mut self, muted: bool) {
        let request = self.request().await;
        assert_eq!(request["method"], "setChannel");
        assert_eq!(request["params"], json!({"id":"channel","isMuted":muted}));
        self.reply(&request, json!({})).await;
        let request = self.request().await;
        assert_eq!(request["method"], "getChannels");
        let mut value = catalog("getChannels");
        value["channels"][0]["isMuted"] = json!(muted);
        self.reply(&request, value).await;
    }
    pub async fn closed(&mut self) {
        timeout(Duration::from_secs(2), async {
            while let Some(message) = self.socket.next().await {
                match message {
                    Err(_) | Ok(Message::Close(_)) => return,
                    Ok(Message::Text(text)) => {
                        let value: Value = serde_json::from_str(text.as_str()).unwrap();
                        assert!(
                            value["method"]
                                .as_str()
                                .is_some_and(|method| method.starts_with("get")),
                            "no mutation may be replayed during teardown: {value}"
                        );
                    }
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap();
    }
}

pub(crate) async fn until(
    wave: &WaveLink,
    condition: impl Fn(&WaveLinkState) -> bool,
) -> WaveLinkState {
    let mut changes = wave.subscribe();
    timeout(Duration::from_secs(5), async {
        loop {
            let state = changes.borrow_and_update().clone();
            if condition(&state) {
                return state;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

pub(crate) fn catalog(method: &str) -> Value {
    match method {
        "getChannels" => {
            json!({"channels":[{"id":"channel","name":"Music","level":0.7,"isMuted":false,"apps":[{"id":"app","name":"App"}],"mixes":[{"id":"mix","level":0.8,"isMuted":false}]}]})
        }
        "getMixes" => {
            json!({"mixes":[{"id":"mix","name":"Monitor","level":1.0,"isMuted":false},{"id":"unpaired","name":"Other"}]})
        }
        "getInputDevices" => {
            json!({"inputDevices":[{"id":"micA","name":"Mic A","inputs":[{"id":"same","isMuted":false,"gain":{"value":0.21666667,"min":20,"max":50,"unit":"decibel"}}]},{"id":"micB","inputs":[{"id":"same"}]}]})
        }
        "getOutputDevices" => {
            json!({"mainOutput":{"outputDeviceId":"outA","outputId":"same"},"outputDevices":[{"id":"outA","outputs":[{"id":"same","isMuted":false,"level":0.5,"mixId":""}]},{"id":"outB","outputs":[{"id":"same","isMuted":true}]}]})
        }
        _ => panic!("unexpected getter {method}"),
    }
}

fn ready_catalog() -> (Catalog, WaveLinkState) {
    let mut data = Catalog::default();
    for domain in Domain::ALL {
        data.replace(domain, catalog(domain.method())).unwrap();
    }
    let mut state = WaveLinkState {
        status: WaveLinkStatus::Ready,
        ..Default::default()
    };
    data.apply(&mut state);
    (data, state)
}

#[test]
fn sparse_qualified_controls_and_gain_remain_honest() {
    let (_, state) = ready_catalog();
    assert_eq!(state.controls.len(), 9);
    let absent = state
        .control(&WaveLinkTarget::Input {
            input_device_id: "micB".into(),
            input_id: "same".into(),
        })
        .unwrap();
    assert_eq!(absent.muted, None);
    assert!(!absent.can_mute);
    assert!(!absent.can_set_level);
    let mute = state
        .control(&WaveLinkTarget::Input {
            input_device_id: "micA".into(),
            input_id: "same".into(),
        })
        .unwrap();
    assert!(mute.can_mute);
    assert!(!mute.can_set_level);
    assert_eq!(mute.level, None);
    let gain = state
        .control(&WaveLinkTarget::InputGain {
            input_device_id: "micA".into(),
            input_id: "same".into(),
        })
        .unwrap();
    assert_eq!(gain.gain.as_ref().unwrap().value, 0.21666667);
    assert_eq!(gain.gain.as_ref().unwrap().min, 20.0);
    assert!(!gain.can_mute && !gain.can_set_level);
    assert_eq!(gain.level, None);
    assert!(state
        .control(&WaveLinkTarget::ChannelMix {
            channel_id: "channel".into(),
            mix_id: "unpaired".into()
        })
        .is_none());
    assert_eq!(state.outputs[0].mix_id.as_deref(), Some(""));
    assert!(state.outputs[0].can_set_mix);
    assert_eq!(state.outputs[1].mix_id, None);
    assert!(!state.outputs[1].can_set_mix);
    let output = state
        .control(&WaveLinkTarget::Output {
            output_device_id: "outB".into(),
            output_id: "same".into(),
        })
        .unwrap();
    assert_eq!(output.level, None);
    assert!(output.can_mute);
    assert!(!output.can_set_level);
}

#[test]
fn typed_partial_nested_patches_preserve_siblings_and_plural_replaces() {
    let (mut data, mut state) = ready_catalog();
    data.patch(
        Domain::Channels,
        json!({"id":"channel","mixes":[{"id":"mix","isMuted":true}]}),
    )
    .unwrap();
    data.patch(
        Domain::Inputs,
        json!({"id":"micA","inputs":[{"id":"same","isMuted":true,"gain":{"value":0.4}}]}),
    )
    .unwrap();
    data.patch(
        Domain::Outputs,
        json!({"id":"outA","outputs":[{"id":"same","level":0.2}]}),
    )
    .unwrap();
    data.apply(&mut state);
    let mix = state
        .control(&WaveLinkTarget::ChannelMix {
            channel_id: "channel".into(),
            mix_id: "mix".into(),
        })
        .unwrap();
    assert_eq!((mix.level, mix.muted), (Some(0.8), Some(true)));
    assert_eq!(state.channels[0].apps.len(), 1);
    let gain = state
        .control(&WaveLinkTarget::InputGain {
            input_device_id: "micA".into(),
            input_id: "same".into(),
        })
        .unwrap()
        .gain
        .as_ref()
        .unwrap();
    assert_eq!(gain.value, 0.4);
    assert_eq!((gain.min, gain.max), (20.0, 50.0));
    assert!(data
        .patch(
            Domain::Outputs,
            json!({"id":"outA","outputs":[{"id":"same","isMuted":"not a bool"}]})
        )
        .is_err());
    assert!(data
        .patch(
            Domain::Channels,
            json!({"id":"channel","mixes":[{"id":"mix"},{"id":"mix"}]})
        )
        .is_err());
    data.replace(Domain::Channels, json!({"channels":[]}))
        .unwrap();
    data.apply(&mut state);
    assert!(state.channels.is_empty());
    assert!(data
        .replace(Domain::Mixes, json!({"unexpected":[]}))
        .is_err());
    data.replace(
        Domain::Mixes,
        json!({"mixes":[{"id":"bad","level":1.2,"isMuted":false,"image":{"imgData":"discard"}}]}),
    )
    .unwrap();
    data.apply(&mut state);
    let bad = state
        .control(&WaveLinkTarget::Mix {
            mix_id: "bad".into(),
        })
        .unwrap();
    assert_eq!(bad.level, None);
    assert!(!bad.can_set_level);
    assert!(!serde_json::to_string(&state).unwrap().contains("discard"));
}

#[tokio::test]
async fn origin_identity_no_version_out_of_order_poll_and_duplicate_connect() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let state = wave.state();
    assert!(state.connected);
    assert_eq!(state.application.unwrap().interface_revision, 2);
    assert_eq!(state.version, None);
    assert_eq!(wave.connect().generation, state.generation);
    let revision = state.revision;
    for _ in 0..4 {
        peer.getter().await;
    }
    assert_eq!(wave.state().generation, state.generation);
    // Identical getter feedback does not increment the publication revision.
    assert_eq!(wave.state().revision, revision);
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn wrong_identity_and_revision_never_become_ready_or_mutate() {
    for (app_id, revision) in [("not-EWL", 2), ("EWL", 1)] {
        let fixture = Fixture::new().await;
        let wave = fixture.start(false);
        wave.connect();
        let mut peer = fixture.accept().await;
        let request = peer.request().await;
        peer.reply(&request,json!({"name":"other","operatingSystem":"mac","appID":app_id,"interfaceRevision":revision})).await;
        let state = until(&wave, |state| state.status == WaveLinkStatus::Unsupported).await;
        assert!(!state.connected && state.controls.is_empty());
        peer.closed().await;
        wave.shutdown();
    }
}

#[tokio::test]
async fn rejected_origin_never_sends_identification_or_catalog_requests() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(false);
    wave.connect();
    let (socket, _) = timeout(Duration::from_secs(2), fixture.listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(tokio_tungstenite::accept_hdr_async(socket, RejectOrigin)
        .await
        .is_err());
    let state = until(&wave, |state| {
        state.status == WaveLinkStatus::Disconnected && state.error.is_some()
    })
    .await;
    assert!(!state.connected);
    assert!(state.controls.is_empty());
    wave.shutdown();
}

#[tokio::test]
async fn discovery_skips_a_wrong_endpoint_and_identifies_the_next_fixture() {
    let bad = Fixture::new().await;
    let good = Fixture::new().await;
    let wave = WaveLink::start_with_ports(
        WaveLinkSettings {
            enabled: true,
            ..Default::default()
        },
        Some(vec![bad.port, good.port]),
    );
    let mut rejected = bad.accept().await;
    let request = rejected.request().await;
    rejected
        .reply(
            &request,
            json!({"name":"wrong","operatingSystem":"mac","appID":"OTHER","interfaceRevision":2}),
        )
        .await;
    let mut peer = good.ready(&wave).await;
    assert_eq!(wave.state().port, Some(good.port));
    rejected.closed().await;
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn disconnect_during_identification_cancels_promptly_and_disable_stays_offline() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.accept().await;
    let request = peer.request().await;
    assert_eq!(request["method"], "getApplicationInfo");
    assert!(wave.state().connected);
    assert_eq!(wave.state().status, WaveLinkStatus::Synchronizing);
    wave.disconnect();
    peer.closed().await;
    assert_eq!(wave.state().status, WaveLinkStatus::Disconnected);
    wave.configure(WaveLinkSettings::default()).unwrap();
    assert_eq!(wave.state().status, WaveLinkStatus::Disabled);
    let generation = wave.state().generation;
    assert!(wave
        .write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into()
                },
                true
            )
        )
        .is_err());
    wave.shutdown();
}

#[tokio::test]
async fn getter_notification_fence_and_partial_feedback_never_send_setters() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    wave.refresh();
    let mut old = Vec::new();
    for _ in 0..4 {
        old.push(peer.request().await);
    }
    peer.notification(
        "channelChanged",
        json!({"id":"channel","level":0.25,"mixes":[{"id":"mix","isMuted":true}]}),
    )
    .await;
    until(&wave, |state| {
        state
            .control(&WaveLinkTarget::Channel {
                channel_id: "channel".into(),
            })
            .is_some_and(|control| control.level == Some(0.25))
    })
    .await;
    for request in old {
        let result = catalog(request["method"].as_str().unwrap());
        peer.reply(&request, result).await;
    }
    let retry = peer.request().await;
    assert_eq!(retry["method"], "getChannels");
    assert_eq!(
        wave.state()
            .controls
            .iter()
            .find(|c| c.target
                == WaveLinkTarget::Channel {
                    channel_id: "channel".into()
                })
            .unwrap()
            .level,
        Some(0.25)
    );
    let mut current = catalog("getChannels");
    current["channels"][0]["level"] = json!(0.25);
    peer.reply(&retry, current).await;
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn setter_requires_authoritative_readback_and_old_generation_cannot_restore() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let generation = wave.state().generation;
    let writer = wave.clone();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(writer.write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into(),
                },
                true,
            ),
        ));
    });
    let request = peer.request().await;
    assert_eq!(request["method"], "setChannel");
    peer.reply(&request, json!({})).await;
    let readback = peer.request().await;
    assert_eq!(readback["method"], "getChannels");
    assert!(matches!(
        done_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let mut value = catalog("getChannels");
    value["channels"][0]["isMuted"] = json!(true);
    peer.reply(&readback, value).await;
    done_rx.await.unwrap().unwrap();
    assert_eq!(
        wave.state()
            .control(&WaveLinkTarget::Channel {
                channel_id: "channel".into()
            })
            .unwrap()
            .muted,
        Some(true)
    );
    wave.disconnect();
    peer.closed().await;
    wave.connect();
    let mut peer = fixture.ready(&wave).await;
    assert!(wave.state().generation > generation);
    assert!(wave
        .write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into()
                },
                false
            )
        )
        .is_err());
    for _ in 0..4 {
        peer.getter().await;
    }
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn prewrite_getter_and_late_response_cannot_replace_postwrite_readback() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    wave.refresh();
    let mut old = Vec::new();
    for _ in 0..4 {
        old.push(peer.request().await);
    }
    let generation = wave.state().generation;
    let writer = wave.clone();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(writer.write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into(),
                },
                true,
            ),
        ));
    });
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setChannel");
    peer.reply(&setter, json!({})).await;
    for request in old {
        peer.reply(&request, catalog(request["method"].as_str().unwrap()))
            .await;
    }
    let readback = peer.request().await;
    assert_eq!(readback["method"], "getChannels");
    assert!(matches!(
        done_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let mut value = catalog("getChannels");
    value["channels"][0]["isMuted"] = json!(true);
    peer.reply(&readback, value).await;
    done_rx.await.unwrap().unwrap();
    assert_eq!(
        wave.state()
            .control(&WaveLinkTarget::Channel {
                channel_id: "channel".into()
            })
            .unwrap()
            .muted,
        Some(true)
    );
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn rpc_write_error_invalidates_capabilities_until_catalog_resync() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let generation = wave.state().generation;
    let writer = wave.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(writer.write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into(),
                },
                true,
            ),
        ));
    });
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setChannel");
    peer.send(json!({"jsonrpc":"2.0","id":setter["id"],"error":{"code":-32601,"message":"fixture unsupported","data":{"ignored":"not logged"}}})).await;
    assert!(done_rx
        .await
        .unwrap()
        .unwrap_err()
        .contains("fixture unsupported"));
    let resync = peer.request().await;
    assert_eq!(resync["method"], "getChannels");
    assert_eq!(wave.state().status, WaveLinkStatus::Synchronizing);
    assert!(wave
        .state()
        .controls
        .iter()
        .all(|control| !control.can_set_level && !control.can_mute));
    peer.reply(&resync, catalog("getChannels")).await;
    until(&wave, |state| state.status == WaveLinkStatus::Ready).await;
    peer.notification(
        "channelChanged",
        json!({"id":"channel","level":"malformed"}),
    )
    .await;
    let resync = peer.request().await;
    assert_eq!(resync["method"], "getChannels");
    assert_eq!(wave.state().status, WaveLinkStatus::Synchronizing);
    peer.reply(&resync, catalog("getChannels")).await;
    until(&wave, |state| state.status == WaveLinkStatus::Ready).await;
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn unsupported_getter_schema_stays_unsupported_until_that_domain_resynchronizes() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    wave.refresh();
    let mut requests = Vec::new();
    for _ in 0..4 {
        requests.push(peer.request().await);
    }
    let channels = requests
        .iter()
        .find(|request| request["method"] == "getChannels")
        .unwrap();
    peer.reply(channels, json!({"broken":[]})).await;
    for request in requests.iter().filter(|request| {
        request["method"] == "getInputDevices" || request["method"] == "getOutputDevices"
    }) {
        peer.reply(request, catalog(request["method"].as_str().unwrap()))
            .await;
    }
    let mixes = requests
        .iter()
        .find(|request| request["method"] == "getMixes")
        .unwrap();
    let mut value = catalog("getMixes");
    value["mixes"][0]["name"] = json!("last getter barrier");
    peer.reply(mixes, value).await;
    let state = until(&wave, |state| {
        state.status == WaveLinkStatus::Unsupported
            && state
                .mixes
                .iter()
                .any(|mix| mix.name == "last getter barrier")
    })
    .await;
    assert!(state.connected);
    assert!(state.error.is_some());
    assert!(state
        .controls
        .iter()
        .all(|control| !control.can_set_level && !control.can_mute));
    for _ in 0..4 {
        peer.getter().await;
    }
    assert_eq!(
        until(&wave, |state| state.status == WaveLinkStatus::Ready)
            .await
            .error,
        None
    );
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn timed_out_mutation_is_unconfirmed_and_never_retried() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let generation = wave.state().generation;
    let writer = wave.clone();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(writer.write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into(),
                },
                true,
            ),
        ));
    });
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setChannel");
    let result = timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result=&mut done_rx=>break result.unwrap(),
                _=peer.getter()=>{},
            }
        }
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("unconfirmed"));
    // Late acknowledgement cannot resurrect the expired request or cause replay.
    peer.reply(&setter, json!({})).await;
    for _ in 0..4 {
        peer.getter().await;
    }
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn unexpected_disconnect_reconnects_without_replaying_unacknowledged_writes() {
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let generation = wave.state().generation;
    let writer = wave.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(writer.write(
            generation,
            Mutation::Mute(
                WaveLinkTarget::Channel {
                    channel_id: "channel".into(),
                },
                true,
            ),
        ));
    });
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setChannel");
    drop(peer);
    assert!(timeout(Duration::from_secs(2), done_rx)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let mut peer = fixture.ready(&wave).await;
    assert!(wave.state().generation > generation);
    for _ in 0..4 {
        peer.getter().await;
    }
    wave.shutdown();
    peer.closed().await;
}
