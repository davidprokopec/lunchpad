use super::*;
use crate::config::FineTuneSettings;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{unix::OwnedWriteHalf, UnixListener};
use tokio::time::timeout;

pub(crate) struct Fixture {
    listener: UnixListener,
    dir: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
pub(crate) struct Peer {
    frames: super::transport::Frames<tokio::net::unix::OwnedReadHalf>,
    writer: OwnedWriteHalf,
    pub session: String,
    pub revision: u64,
    pub snapshot: Value,
}
impl Fixture {
    pub async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ft-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("s")).unwrap();
        Self { listener, dir }
    }
    pub fn start(&self, auto_connect: bool) -> FineTune {
        FineTune::start_at(
            FineTuneSettings {
                enabled: true,
                auto_connect,
            },
            Some(self.dir.join("s")),
        )
    }
    pub async fn accept(&self) -> Peer {
        let (socket, _) = timeout(Duration::from_secs(5), self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (reader, writer) = socket.into_split();
        Peer {
            frames: super::transport::Frames::new(reader),
            writer,
            session: "fixture-session".into(),
            revision: 0,
            snapshot: snapshot(),
        }
    }
    pub async fn ready(&self, fine: &FineTune) -> Peer {
        self.ready_session(fine, "fixture-session").await
    }
    pub async fn ready_session(&self, fine: &FineTune, session: &str) -> Peer {
        let mut peer = self.accept().await;
        peer.session = session.into();
        peer.hello().await;
        let request = peer.request().await;
        assert_eq!(request["method"], "subscribe");
        assert_eq!(request["params"], json!({}));
        peer.snapshot_reply(&request).await;
        until(fine, |state| state.status == FineTuneStatus::Ready).await;
        peer
    }
    pub async fn ready_managed(&self, fine: &FineTune) -> Peer {
        let mut peer = self.accept().await;
        peer.snapshot = managed_snapshot(false, false);
        peer.hello_managed().await;
        let request = peer.request().await;
        assert_eq!(request["method"], "subscribe");
        peer.snapshot_reply(&request).await;
        until(fine, |state| state.status == FineTuneStatus::Ready).await;
        peer
    }
}
impl Peer {
    pub async fn hello(&mut self) {
        self.send(json!({"type":"hello","protocol":"finetune-control","version":1,"appVersion":"fixture","sessionId":self.session,"capabilities":["getState","subscribe","setAppVolume","setAppMute","setAppRouting","setDefaultOutput","setPreferredInput"]})).await;
    }
    pub async fn hello_managed(&mut self) {
        self.send(json!({"type":"hello","protocol":"finetune-control","version":1,"appVersion":"fixture","sessionId":self.session,"capabilities":["getState","subscribe","setAppVolume","setAppMute","setAppRouting","setDefaultOutput","setPreferredInput","managedApps"]})).await;
    }
    pub async fn send(&mut self, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
    }
    pub async fn request(&mut self) -> Value {
        let frame = timeout(Duration::from_secs(5), self.frames.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame["type"], "request");
        assert_eq!(frame["sessionId"], self.session);
        assert!(frame["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && id.len() <= 128));
        frame
    }
    pub async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"type":"response","id":request["id"],"result":result}))
            .await;
    }
    pub async fn snapshot_reply(&mut self, request: &Value) {
        self.reply(
            request,
            json!({"sessionId":self.session,"revision":self.revision,"state":self.snapshot}),
        )
        .await;
    }
    pub async fn event(&mut self) {
        self.send(json!({"type":"event","event":"stateChanged","sessionId":self.session,"revision":self.revision,"state":self.snapshot})).await;
    }
    pub async fn mutation_readback(&mut self, muted: bool) {
        let setter = self.request().await;
        assert_eq!(setter["method"], "setAppMute");
        let managed = self.snapshot.get("managedApps").is_some();
        let expected = if managed {
            json!({"appId":"app","muted":muted})
        } else {
            json!({"appId":"app","pid":123,"muted":muted})
        };
        assert_eq!(setter["params"], expected);
        self.reply(&setter, json!({"accepted":true})).await;
        let getter = self.request().await;
        assert_eq!(getter["method"], "getState");
        self.snapshot[if managed { "managedApps" } else { "apps" }][0]["muted"] = json!(muted);
        self.revision += 1;
        self.snapshot_reply(&getter).await;
    }
    pub async fn getter(&mut self) {
        let request = self.request().await;
        assert_eq!(
            request["method"], "getState",
            "feedback/teardown must not mutate: {request}"
        );
        self.snapshot_reply(&request).await;
    }
    pub async fn closed(&mut self) {
        timeout(Duration::from_secs(2), async {
            loop {
                match self.frames.next().await {
                    Err(_) => break,
                    Ok(frame) => assert_eq!(
                        frame["method"], "getState",
                        "no mutation during teardown: {frame}"
                    ),
                }
            }
        })
        .await
        .unwrap();
    }
}

pub(crate) async fn until(
    fine: &FineTune,
    condition: impl Fn(&FineTuneState) -> bool,
) -> FineTuneState {
    let mut changes = fine.subscribe();
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

pub(crate) fn snapshot() -> Value {
    json!({
        "apps":[{"appId":"app","pids":[123],"name":"Player","active":true,"volume":0.7,"boost":1.0,"muted":false,"routing":{"requested":{"mode":"followDefault"},"appliedDeviceUIDs":["out"],"status":"applied","error":null}}],
        "devices":[
            {"uid":"out","name":"Speakers","direction":"output","isDefault":true,"volume":0.75,"muted":false,"volumeSource":"hardware","capabilities":{"volumeReadable":true,"muteReadable":true,"selectable":true}},
            {"uid":"in","name":"Mic","direction":"input","isDefault":true,"volume":null,"muted":null,"volumeSource":null,"capabilities":{"volumeReadable":false,"muteReadable":false,"selectable":true}}
        ],
        "defaultOutput":"out","defaultInput":"in","inputPolicy":{"lockEnabled":true,"preferredDeviceUID":"in","lockedDeviceUID":"in"},"permission":{"audioCapture":"authorized","source":"preflight"}
    })
}

fn managed_snapshot(active: bool, running: bool) -> Value {
    let mut state = snapshot();
    let mut app = state["apps"][0].clone();
    app["active"] = json!(active);
    app["running"] = json!(running);
    app["manageable"] = json!(true);
    app["pids"] = if running { json!([123]) } else { json!([]) };
    if !active {
        app["routing"] = json!({"requested":{"mode":"followDefault"},"appliedDeviceUIDs":null,"status":"unavailable","error":null});
        state["apps"] = json!([]);
    }
    state["managedApps"] = json!([app]);
    state
}

#[tokio::test]
async fn managed_capability_requires_valid_catalogue_without_relaxing_legacy_apps() {
    let mut missing = snapshot();
    missing.as_object_mut().unwrap().remove("managedApps");
    let mut missing_flag = managed_snapshot(false, false);
    missing_flag["managedApps"][0]
        .as_object_mut()
        .unwrap()
        .remove("manageable");
    let mut zero_pid = managed_snapshot(false, true);
    zero_pid["managedApps"][0]["pids"] = json!([0]);
    let mut inconsistent = managed_snapshot(true, true);
    inconsistent["managedApps"][0]["running"] = json!(false);
    let mut invalid_legacy = managed_snapshot(false, false);
    invalid_legacy["apps"] = json!([invalid_legacy["managedApps"][0].clone()]);
    let mut null_catalogue = snapshot();
    null_catalogue["managedApps"] = Value::Null;
    let mut invalid_catalogue = snapshot();
    invalid_catalogue["managedApps"] = json!({});
    for snapshot in [
        missing,
        missing_flag,
        zero_pid,
        inconsistent,
        invalid_legacy,
        null_catalogue,
        invalid_catalogue,
    ] {
        let fixture = Fixture::new().await;
        let fine = fixture.start(false);
        fine.connect();
        let mut peer = fixture.accept().await;
        peer.snapshot = snapshot;
        peer.hello_managed().await;
        let request = peer.request().await;
        peer.snapshot_reply(&request).await;
        let failed = until(&fine, |state| state.status == FineTuneStatus::Disconnected).await;
        assert!(failed.error.is_some());
        assert!(failed.snapshot.is_none());
        assert!(failed.capabilities.is_empty());
        peer.closed().await;
        fine.shutdown();
    }
}

#[tokio::test]
async fn legacy_server_ignores_unadvertised_managed_catalogue() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.accept().await;
    peer.snapshot = managed_snapshot(false, false);
    peer.hello().await;
    let request = peer.request().await;
    peer.snapshot_reply(&request).await;
    until(&fine, |state| state.status == FineTuneStatus::Ready).await;
    assert!(!fine.state().supports_managed_apps());
    assert!(fine.state().app("app").unwrap_err().contains("missing"));
    let handle = fine.clone();
    let generation = fine.state().generation;
    assert!(tokio::task::spawn_blocking(
        move || handle.write(generation, Mutation::Mute("app".into(), true))
    )
    .await
    .unwrap()
    .unwrap_err()
    .contains("missing"));
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn managed_inactive_and_active_dispatch_use_authoritative_identity_and_readback() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready_managed(&fine).await;
    assert!(fine.state().supports_managed_apps());
    let generation = fine.state().generation;
    for (active, running, pid) in [(false, false, 0), (false, true, 222), (true, true, 333)] {
        peer.snapshot = managed_snapshot(active, running);
        if running {
            peer.snapshot["managedApps"][0]["pids"] = json!([pid]);
        }
        if active {
            peer.snapshot["apps"][0]["pids"] = json!([pid]);
        }
        peer.revision += 1;
        peer.event().await;
        until(&fine, |state| state.server_revision == Some(peer.revision)).await;
        for mute in [false, true] {
            let handle = fine.clone();
            let mutation = if mute {
                Mutation::Mute("app".into(), true)
            } else {
                Mutation::Volume("app".into(), 0.25)
            };
            let write = tokio::task::spawn_blocking(move || handle.write(generation, mutation));
            let setter = peer.request().await;
            assert_eq!(setter["params"]["appId"], "app");
            if active {
                assert_eq!(setter["params"]["pid"], pid);
            } else {
                assert!(setter["params"].get("pid").is_none());
            }
            assert_eq!(
                setter["method"],
                if mute { "setAppMute" } else { "setAppVolume" }
            );
            assert_eq!(
                setter["params"][if mute { "muted" } else { "volume" }],
                if mute { json!(true) } else { json!(0.25) }
            );
            peer.reply(&setter, json!({"accepted":true})).await;
            let readback = peer.request().await;
            assert_eq!(readback["method"], "getState");
            assert!(!fine.state().app("app").unwrap().muted);
            assert_eq!(fine.state().app("app").unwrap().volume, 0.7);
            peer.snapshot_reply(&readback).await;
            write.await.unwrap().unwrap();
        }
    }
    fine.disconnect();
    assert!(fine.state().capabilities.is_empty());
    peer.closed().await;
    fine.connect();
    let mut peer = fixture.ready(&fine).await;
    assert!(!fine.state().supports_managed_apps());
    let handle = fine.clone();
    assert!(tokio::task::spawn_blocking(
        move || handle.write(generation, Mutation::Mute("app".into(), true))
    )
    .await
    .unwrap()
    .is_err());
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn managed_readability_is_independent_of_write_and_routing_eligibility() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready_managed(&fine).await;
    let generation = fine.state().generation;
    for (kind, reason) in [
        ("denied", "permission"),
        ("ignored", "manageable"),
        ("duplicate", "ambiguous"),
        ("missing", "missing"),
        ("inactive", "active"),
    ] {
        peer.snapshot = managed_snapshot(false, false);
        match kind {
            "denied" => peer.snapshot["permission"]["audioCapture"] = json!("denied"),
            "ignored" => peer.snapshot["managedApps"][0]["manageable"] = json!(false),
            "duplicate" => {
                let app = peer.snapshot["managedApps"][0].clone();
                peer.snapshot["managedApps"] = json!([app.clone(), app]);
            }
            "missing" => peer.snapshot["managedApps"] = json!([]),
            _ => {}
        }
        peer.revision += 1;
        peer.event().await;
        let state = until(&fine, |state| state.server_revision == Some(peer.revision)).await;
        if matches!(kind, "denied" | "ignored" | "inactive") {
            assert_eq!(state.app("app").unwrap().volume, 0.7);
            let readable = crate::system_audio::fine_tune_state(
                &state,
                &FineTuneTarget::App {
                    app_id: "app".into(),
                },
            )
            .unwrap();
            assert_eq!(readable.can_set_volume, kind == "inactive");
            assert_eq!(readable.can_mute, kind == "inactive");
        } else {
            assert!(state.app("app").is_err());
        }
        let mutations = if kind == "inactive" {
            vec![Mutation::Routing(
                "app".into(),
                FineTuneRoutingRequest::FollowDefault,
            )]
        } else {
            vec![
                Mutation::Volume("app".into(), 0.5),
                Mutation::Mute("app".into(), true),
            ]
        };
        for mutation in mutations {
            let handle = fine.clone();
            let error = tokio::task::spawn_blocking(move || handle.write(generation, mutation))
                .await
                .unwrap()
                .unwrap_err();
            assert!(error.contains(reason), "{kind}: {error}");
        }
        fine.refresh();
        peer.getter().await;
    }
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn hello_first_subscribe_then_revisioned_full_snapshots() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.accept().await;
    let mut byte = [0];
    assert_eq!(
        peer.frames
            .reader
            .get_ref()
            .try_read(&mut byte)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    peer.hello().await;
    let subscribe = peer.request().await;
    assert_eq!(subscribe["method"], "subscribe");
    assert_eq!(fine.state().status, FineTuneStatus::Synchronizing);
    peer.snapshot_reply(&subscribe).await;
    let initial = until(&fine, |state| state.status == FineTuneStatus::Ready).await;
    assert_eq!(initial.server_revision, Some(0));
    peer.snapshot["apps"][0]["volume"] = json!(0.2);
    peer.revision = 2;
    peer.event().await;
    until(&fine, |state| state.server_revision == Some(2)).await;
    peer.snapshot["apps"] = json!([]);
    peer.revision = 1;
    peer.event().await;
    peer.revision = 2;
    peer.event().await;
    fine.refresh();
    peer.getter().await;
    // A request/response barrier after both obsolete events establishes receipt order.
    let handle = fine.clone();
    let write = tokio::task::spawn_blocking(move || {
        handle.write(initial.generation, Mutation::Mute("app".into(), true))
    });
    let request = peer.request().await;
    assert_eq!(request["params"]["pid"], 123);
    assert_eq!(fine.state().app("app").unwrap().volume, 0.2);
    peer.send(json!({"type":"response","id":request["id"],"error":{"code":"commandFailed","message":"fixture"}})).await;
    assert!(write.await.unwrap().is_err());
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn disabled_integration_never_connects_and_enable_disable_reenable_is_fenced() {
    let fixture = Fixture::new().await;
    let fine = FineTune::start_at(FineTuneSettings::default(), Some(fixture.dir.join("s")));
    fine.barrier().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disabled);
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());
    fine.connect();
    fine.refresh();
    fine.barrier().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disabled);
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());

    fine.configure(FineTuneSettings {
        enabled: true,
        auto_connect: true,
    });
    let mut peer = fixture.ready(&fine).await;
    let ready = fine.state();
    let target = FineTuneTarget::App {
        app_id: "app".into(),
    };
    assert!(ready.target_reason(&target).is_none());
    let handle = fine.clone();
    let pending = tokio::task::spawn_blocking(move || {
        handle.write(ready.generation, Mutation::Mute("app".into(), true))
    });
    let request = peer.request().await;
    assert_eq!(request["method"], "setAppMute");
    fine.configure(FineTuneSettings::default());
    let disabled = fine.state();
    assert_eq!(disabled.status, FineTuneStatus::Disabled);
    assert!(!disabled.connected);
    assert!(disabled.snapshot.is_none());
    assert!(disabled.session_id.is_none());
    assert!(disabled.target_reason(&target).is_some());
    assert!(disabled
        .target_reason(&FineTuneTarget::Output {
            device_uid: "out".into()
        })
        .is_some());
    assert!(disabled
        .target_reason(&FineTuneTarget::Input {
            device_uid: "in".into()
        })
        .is_some());
    assert!(fine
        .write(ready.generation, Mutation::Mute("app".into(), true))
        .is_err());
    peer.closed().await;
    assert!(timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    fine.barrier().await;
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());

    fine.configure(FineTuneSettings {
        enabled: true,
        auto_connect: true,
    });
    let mut peer = fixture.ready_session(&fine, "reenabled").await;
    assert!(fine.state().generation > ready.generation);
    assert!(fine
        .write(ready.generation, Mutation::Mute("app".into(), true))
        .is_err());
    fine.refresh();
    peer.getter().await;
    fine.configure(FineTuneSettings::default());
    peer.closed().await;
    fine.barrier().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disabled);
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());
    fine.shutdown();
}

#[tokio::test]
async fn disabling_cancels_handshake_and_backoff_without_a_reconnect() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.accept().await;
    // No hello: disable must cancel the reader rather than wait for its timeout.
    fine.configure(FineTuneSettings::default());
    peer.closed().await;
    fine.barrier().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disabled);
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());

    fine.configure(FineTuneSettings {
        enabled: true,
        auto_connect: true,
    });
    let peer = fixture.ready_session(&fine, "before-backoff").await;
    drop(peer);
    until(&fine, |state| state.status == FineTuneStatus::Disconnected).await;
    fine.configure(FineTuneSettings::default());
    fine.barrier().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disabled);
    assert!(futures_util::poll!(Box::pin(fixture.listener.accept())).is_pending());

    fine.configure(FineTuneSettings {
        enabled: true,
        auto_connect: true,
    });
    let mut peer = fixture.ready_session(&fine, "after-backoff").await;
    fine.configure(FineTuneSettings::default());
    peer.closed().await;
    fine.barrier().await;
    fine.shutdown();
}

#[tokio::test]
async fn unsupported_hello_versions_never_subscribe() {
    for (protocol, version) in [("other", 1), ("finetune-control", 2)] {
        let fixture = Fixture::new().await;
        let fine = fixture.start(true);
        let mut peer = fixture.accept().await;
        peer.send(json!({"type":"hello","protocol":protocol,"version":version}))
            .await;
        let state = until(&fine, |state| state.status == FineTuneStatus::Unsupported).await;
        assert!(!state.connected);
        assert!(state.snapshot.is_none());
        peer.closed().await;
        fine.connect();
        let mut peer = fixture.ready(&fine).await;
        fine.shutdown();
        peer.closed().await;
    }
}

#[tokio::test]
async fn subscribe_response_must_precede_events() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(false);
    fine.connect();
    let mut peer = fixture.accept().await;
    peer.hello().await;
    let request = peer.request().await;
    assert_eq!(request["method"], "subscribe");
    peer.event().await;
    until(&fine, |state| state.status == FineTuneStatus::Disconnected).await;
    assert!(fine.state().snapshot.is_none());
    peer.closed().await;
    fine.shutdown();
}

#[tokio::test]
async fn accepted_is_not_applied_and_current_pid_is_resolved_at_dispatch() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready(&fine).await;
    let generation = fine.state().generation;
    peer.snapshot["apps"][0]["pids"] = json!([456]);
    peer.revision += 1;
    peer.event().await;
    until(&fine, |state| {
        state.app("app").is_ok_and(|app| app.pids == vec![456])
    })
    .await;
    let handle = fine.clone();
    let write = tokio::task::spawn_blocking(move || {
        handle.write(generation, Mutation::Mute("app".into(), true))
    });
    let request = peer.request().await;
    assert_eq!(
        request["params"],
        json!({"appId":"app","pid":456,"muted":true})
    );
    peer.reply(&request, json!({"accepted":true})).await;
    let readback = peer.request().await;
    assert_eq!(readback["method"], "getState");
    assert!(!fine.state().app("app").unwrap().muted);
    // Even the flushed full snapshot may still be unchanged: no optimistic mute.
    peer.snapshot_reply(&readback).await;
    write.await.unwrap().unwrap();
    assert!(!fine.state().app("app").unwrap().muted);
    peer.revision += 1;
    peer.snapshot["apps"][0]["muted"] = json!(true);
    peer.event().await;
    until(&fine, |state| state.app("app").is_ok_and(|app| app.muted)).await;
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn missing_ambiguous_permission_and_device_validation_never_write() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready(&fine).await;
    let generation = fine.state().generation;
    for (apps, reason) in [
        (json!([]), "missing"),
        (
            json!([snapshot()["apps"][0].clone(), snapshot()["apps"][0].clone()]),
            "ambiguous",
        ),
    ] {
        peer.snapshot["apps"] = apps;
        peer.revision += 1;
        peer.event().await;
        until(&fine, |state| state.server_revision == Some(peer.revision)).await;
        let handle = fine.clone();
        let error = tokio::task::spawn_blocking(move || {
            handle.write(generation, Mutation::Volume("app".into(), 0.5))
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.contains(reason), "{error}");
        assert!(fine
            .state()
            .target_reason(&FineTuneTarget::App {
                app_id: "app".into()
            })
            .unwrap()
            .contains(reason));
    }
    peer.snapshot = snapshot();
    peer.snapshot["permission"]["audioCapture"] = json!("denied");
    peer.revision += 1;
    peer.event().await;
    until(&fine, |state| state.server_revision == Some(peer.revision)).await;
    let handle = fine.clone();
    assert!(tokio::task::spawn_blocking(
        move || handle.write(generation, Mutation::Mute("app".into(), true))
    )
    .await
    .unwrap()
    .unwrap_err()
    .contains("permission"));
    for mutation in [
        Mutation::DefaultOutput("in".into()),
        Mutation::PreferredInput("missing".into()),
    ] {
        let handle = fine.clone();
        assert!(
            tokio::task::spawn_blocking(move || handle.write(generation, mutation))
                .await
                .unwrap()
                .is_err()
        );
    }
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn reconnect_clears_state_and_new_session_rejects_old_generation() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let peer = fixture.ready(&fine).await;
    let generation = fine.state().generation;
    drop(peer);
    until(&fine, |state| !state.connected && state.snapshot.is_none()).await;
    let mut peer = fixture.ready_session(&fine, "new-session").await;
    assert!(fine.state().generation > generation);
    assert_eq!(fine.state().server_revision, Some(0));
    assert_eq!(fine.state().session_id.as_deref(), Some("new-session"));
    assert!(fine
        .write(generation, Mutation::Mute("app".into(), true))
        .is_err());
    fine.refresh();
    peer.getter().await;
    fine.disconnect();
    peer.closed().await;
    assert_eq!(fine.state().status, FineTuneStatus::Disconnected);
    fine.shutdown();
}

#[tokio::test]
async fn fragmented_coalesced_crlf_and_invalid_frames_are_bounded() {
    let (client, mut server) = tokio::io::duplex(128);
    let mut frames = super::transport::Frames::new(client);
    let (sent, barrier) = tokio::sync::oneshot::channel();
    let (resume, held) = tokio::sync::oneshot::channel();
    let write = tokio::spawn(async move {
        server.write_all(b"{\"a\":").await.unwrap();
        let _ = sent.send(());
        held.await.unwrap();
        server.write_all(b"1}\r\n{\"b\":2}\n").await.unwrap();
    });
    barrier.await.unwrap();
    assert!(futures_util::poll!(Box::pin(frames.next())).is_pending());
    // Cancelling next() must retain the partial frame in the reader itself.
    resume.send(()).unwrap();
    assert_eq!(frames.next().await.unwrap(), json!({"a":1}));
    assert_eq!(frames.next().await.unwrap(), json!({"b":2}));
    write.await.unwrap();
    for bytes in [
        b"\n".to_vec(),
        b"not-json\n".to_vec(),
        vec![0xff, b'\n'],
        b"[]\n".to_vec(),
        vec![b'x'; super::transport::FRAME_LIMIT + 1],
    ] {
        let (client, mut server) = tokio::io::duplex(8192);
        let write = tokio::spawn(async move {
            let _ = server.write_all(&bytes).await;
        });
        let mut frames = super::transport::Frames::new(client);
        assert!(timeout(Duration::from_secs(2), frames.next())
            .await
            .unwrap()
            .is_err());
        drop(frames);
        write.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_and_oversized_socket_frames_clear_ready() {
    for bytes in [
        b"bad\n".to_vec(),
        vec![b'x'; super::transport::FRAME_LIMIT + 1],
    ] {
        let fixture = Fixture::new().await;
        let fine = fixture.start(false);
        fine.connect();
        let mut peer = fixture.ready(&fine).await;
        let _ = peer.writer.write_all(&bytes).await;
        let state = until(&fine, |state| state.status == FineTuneStatus::Disconnected).await;
        assert!(!state.connected);
        assert!(state.snapshot.is_none());
        peer.closed().await;
        fine.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_requests_are_bounded_time_out_and_are_not_replayed() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(false);
    fine.connect();
    let mut peer = fixture.ready(&fine).await;
    let generation = fine.state().generation;
    let mut writes = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for _ in 0..32 {
        let handle = fine.clone();
        writes.push(tokio::task::spawn_blocking(move || {
            handle.write(generation, Mutation::Mute("app".into(), true))
        }));
        let request = peer.request().await;
        assert_eq!(request["method"], "setAppMute");
        assert!(ids.insert(request["id"].as_str().unwrap().to_owned()));
    }
    let handle = fine.clone();
    let error = tokio::task::spawn_blocking(move || {
        handle.write(generation, Mutation::Mute("app".into(), true))
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.contains("limit"), "{error}");
    until(&fine, |state| state.status == FineTuneStatus::Disconnected).await;
    for write in writes {
        assert!(write.await.unwrap().is_err());
    }
    peer.closed().await;
    fine.connect();
    let mut peer = fixture.ready_session(&fine, "after-timeout").await;
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[test]
fn old_settings_and_profile_targets_remain_compatible() {
    let settings: crate::config::Settings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(settings.fine_tune, FineTuneSettings::default());
    assert!(!settings.audio.macos_app_volume);
    assert!(!settings.wave_link.enabled);
    for value in [
        json!({"type":"app","appId":"app"}),
        json!({"type":"output","deviceUid":"out"}),
        json!({"type":"input","deviceUid":"in"}),
    ] {
        let target: FineTuneTarget = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(target).unwrap(), value);
    }
    let old: crate::profile::AudioTarget =
        serde_json::from_value(json!({"type":"app","app":{"id":"app"}})).unwrap();
    assert!(matches!(old, crate::profile::AudioTarget::App { .. }));
}

#[tokio::test]
async fn routing_and_device_selection_use_exact_wire_fields_and_snapshot_policy() {
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready(&fine).await;
    let generation = fine.state().generation;
    for (mutation, method, params) in [
        (
            Mutation::Routing("app".into(), FineTuneRoutingRequest::FollowDefault),
            "setAppRouting",
            json!({"appId":"app","pid":123,"routing":{"mode":"followDefault"}}),
        ),
        (
            Mutation::Routing(
                "app".into(),
                FineTuneRoutingRequest::Single {
                    device_uid: "out".into(),
                },
            ),
            "setAppRouting",
            json!({"appId":"app","pid":123,"routing":{"mode":"single","deviceUID":"out"}}),
        ),
        (
            Mutation::DefaultOutput("out".into()),
            "setDefaultOutput",
            json!({"deviceUID":"out"}),
        ),
        (
            Mutation::PreferredInput("in".into()),
            "setPreferredInput",
            json!({"deviceUID":"in"}),
        ),
    ] {
        let handle = fine.clone();
        let write = tokio::task::spawn_blocking(move || handle.write(generation, mutation));
        let request = peer.request().await;
        assert_eq!(request["method"], method);
        assert_eq!(request["params"], params);
        peer.reply(&request, json!({"accepted":true})).await;
        peer.getter().await;
        write.await.unwrap().unwrap();
    }
    assert!(fine.state().snapshot.unwrap().input_policy.lock_enabled);
    peer.snapshot["apps"][0]["routing"]["requested"] = json!({"mode":"multi","deviceUIDs":["out"]});
    peer.revision += 1;
    peer.event().await;
    until(&fine, |state| state.server_revision == Some(peer.revision)).await;
    let handle = fine.clone();
    assert!(tokio::task::spawn_blocking(move || handle.write(
        generation,
        Mutation::Routing("app".into(), FineTuneRoutingRequest::FollowDefault)
    ))
    .await
    .unwrap()
    .unwrap_err()
    .contains("multi"));
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[test]
fn full_snapshot_schema_requires_nullable_fields_and_roundtrips_exact_uid_keys() {
    let value = snapshot();
    let snapshot: FineTuneSnapshot = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(), value);
    for field in ["defaultOutput", "defaultInput"] {
        let mut invalid = value.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<FineTuneSnapshot>(invalid).is_err());
    }
    let mut invalid = value;
    invalid["devices"][0]
        .as_object_mut()
        .unwrap()
        .remove("muted");
    assert!(serde_json::from_value::<FineTuneSnapshot>(invalid).is_err());
}
