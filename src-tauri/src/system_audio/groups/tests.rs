use super::*;
use serde::Deserialize;

fn member(id: &str) -> AudioMemberTarget {
    AudioMemberTarget::Device {
        device: DeviceRef::Named {
            id: id.into(),
            kind: AudioDeviceKind::Output,
            label: id.into(),
        },
    }
}
fn group(members: Vec<AudioMemberTarget>) -> AudioGroup {
    AudioGroup {
        id: "g".into(),
        name: "Group".into(),
        members,
    }
}
fn target() -> AudioTarget {
    AudioTarget::Group {
        group_id: "g".into(),
    }
}
fn setup(fake: Arc<dyn AudioControl>, members: Vec<AudioMemberTarget>) -> SystemAudio {
    let audio = SystemAudio::new(fake, SharedLive::default());
    audio.configure_groups(&[group(members)]);
    audio.refresh_groups();
    audio
}

async fn barrier(
    audio: &SystemAudio,
) -> (
    std::sync::mpsc::Sender<()>,
    tokio::task::JoinHandle<Result<(), String>>,
) {
    let (entered, ready) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let writer = audio.clone();
    let job = tokio::spawn(async move {
        writer
            .run_writer(move |_| {
                let _ = entered.send(());
                blocked.recv().map_err(|e| e.to_string())
            })
            .await
    });
    ready.await.unwrap();
    (release, job)
}

#[test]
fn shared_summary_fixtures_match_rust_and_typescript() {
    #[derive(Deserialize)]
    struct Member {
        volume: Option<f64>,
        muted: Option<bool>,
    }
    #[derive(Deserialize)]
    struct Fixture {
        name: String,
        members: Vec<Member>,
        expected: AudioGroupSummary,
    }
    let fixtures: Vec<Fixture> = serde_json::from_str(include_str!(
        "../../../../tools/fixtures/audio-group-summary.json"
    ))
    .unwrap();
    for fixture in fixtures {
        let members: Vec<_> = fixture
            .members
            .into_iter()
            .enumerate()
            .map(|(index, m)| AudioGroupMemberReport {
                target: member(&index.to_string()),
                volume: m.volume,
                muted: m.muted,
                can_set_volume: false,
                can_mute: false,
                reason: Some("read-only fixture".into()),
            })
            .collect();
        assert_eq!(
            aggregate_group(&members),
            fixture.expected,
            "{}",
            fixture.name
        );
    }
}

#[test]
fn concurrent_report_publications_have_strictly_increasing_revisions_and_no_writes() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.5)],
        vec![],
    ));
    let audio = setup(fake.clone(), vec![member("out")]);
    let revisions = Arc::new(Mutex::new(Vec::new()));
    let received = revisions.clone();
    audio.set_group_notifier(Arc::new(move |reports| {
        received.lock().push(reports[0].revision)
    }));
    std::thread::scope(|scope| {
        for _ in 0..5 {
            scope.spawn(|| {
                for _ in 0..4 {
                    audio.refresh_groups();
                }
            });
        }
    });
    let revisions = revisions.lock();
    assert_eq!(revisions.len(), 20);
    assert!(revisions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn unavailable_first_does_not_block_healthy_and_aliases_write_once() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = setup(
        fake.clone(),
        vec![
            member("missing"),
            member("out"),
            AudioMemberTarget::Device {
                device: DeviceRef::DefaultOutput,
            },
        ],
    );
    audio
        .apply(target(), SetAudioMode::SetVolume, 25.0)
        .await
        .unwrap();
    assert_eq!(fake.writes(), vec![FakeWrite::Volume("out".into(), 0.25)]);
    let report = audio.group_report("g").unwrap();
    assert!(!report.busy && !report.restore_owned);
    let members = report.operation.unwrap().members;
    assert_eq!(members[0].outcome, AudioGroupOutcome::Skipped);
    assert_eq!(members[1].outcome, AudioGroupOutcome::Confirmed);
    assert_eq!(
        members[2].message.as_deref(),
        Some("duplicate resolved endpoint")
    );
    assert_eq!(report.summary.coverage, AudioGroupCoverage::Partial);
}

#[tokio::test]
async fn missing_at_admission_never_queues_or_replays_after_appearance() {
    let fake = Arc::new(FakeAudio::new(vec![], vec![]));
    let audio = setup(fake.clone(), vec![member("out")]);
    let (release, blocked) = barrier(&audio).await;
    assert!(audio
        .apply(target(), SetAudioMode::SetVolume, 25.0)
        .await
        .is_err());
    assert_eq!(audio.jobs.len(), 0);
    fake.change(|devices, _| {
        devices.push(FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7))
    });
    audio.refresh_groups();
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    audio.run_writer(|_| Ok(())).await.unwrap();
    assert!(fake.writes().is_empty());
    assert!(!audio.group_report("g").unwrap().busy);
}

#[tokio::test]
async fn native_absolute_volume_uses_current_logical_id_but_pins_default_alias() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("old", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = setup(
        fake.clone(),
        vec![AudioMemberTarget::Device {
            device: DeviceRef::DefaultOutput,
        }],
    );
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    fake.change(|devices, _| {
        devices.clear();
        devices.push(FakeAudio::device("new", AudioDeviceKind::Output, true, 0.9));
        devices.push(FakeAudio::device(
            "old",
            AudioDeviceKind::Output,
            false,
            0.8,
        ));
    });
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(fake.writes(), vec![FakeWrite::Volume("old".into(), 0.25)]);
}

#[tokio::test]
async fn admitted_member_missing_at_execution_is_skipped_without_reappearance_write() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = setup(fake.clone(), vec![member("out")]);
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    fake.change(|devices, _| devices.clear());
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert!(response.await.unwrap().is_err());
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Skipped
    );
    fake.change(|devices, _| {
        devices.push(FakeAudio::device("out", AudioDeviceKind::Output, true, 0.8))
    });
    audio.refresh_groups();
    audio.run_writer(|_| Ok(())).await.unwrap();
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn native_app_replacement_same_id_receives_admitted_absolute_value() {
    let app = AudioApp {
        id: "app".into(),
        name: "first".into(),
        volume: 0.7,
        muted: false,
        active: true,
        present: true,
    };
    let fake = Arc::new(FakeAudio::new(vec![], vec![app]));
    let audio = setup(
        fake.clone(),
        vec![AudioMemberTarget::App {
            app: AppRef {
                id: "app".into(),
                name: "first".into(),
            },
        }],
    );
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    fake.change(|_, apps| {
        let mut replacement = apps.remove(0);
        replacement.name = "replacement".into();
        replacement.volume = 0.9;
        apps.push(replacement);
    });
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(fake.writes(), vec![FakeWrite::Volume("app".into(), 0.25)]);
    assert_eq!(fake.apps().unwrap()[0].name, "replacement");
}

#[tokio::test]
async fn explicit_volume_preserves_mute_fader_unmutes_then_sets_and_busy_is_current_only() {
    let mut device = FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7);
    device.muted = true;
    let fake = Arc::new(FakeAudio::new(vec![device], vec![]));
    let audio = setup(fake.clone(), vec![member("out")]);
    audio
        .apply(target(), SetAudioMode::SetVolume, 25.0)
        .await
        .unwrap();
    assert!(fake.devices().unwrap()[0].muted);
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.4, true).unwrap();
    assert!(audio.group_report("g").unwrap().busy);
    assert!(audio.start_group_volume("g", 0.9, true).is_err());
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(
        fake.writes(),
        vec![
            FakeWrite::Volume("out".into(), 0.25),
            FakeWrite::Muted("out".into(), false),
            FakeWrite::Volume("out".into(), 0.4)
        ]
    );
    assert!(!audio.group_report("g").unwrap().busy);
}

struct FailingAudio {
    fake: Arc<FakeAudio>,
    mute_fails: bool,
    volume_fails: bool,
}
impl AudioControl for FailingAudio {
    fn status(&self) -> AudioStatus {
        self.fake.status()
    }
    fn devices(&self) -> Result<Vec<AudioDevice>, String> {
        self.fake.devices()
    }
    fn apps(&self) -> Result<Vec<AudioApp>, String> {
        self.fake.apps()
    }
    fn set_device_volume(&self, id: &str, kind: AudioDeviceKind, value: f32) -> Result<(), String> {
        if id == "bad" && self.volume_fails {
            Err("mock volume failure".into())
        } else {
            self.fake.set_device_volume(id, kind, value)
        }
    }
    fn set_device_muted(&self, id: &str, kind: AudioDeviceKind, value: bool) -> Result<(), String> {
        if id == "bad" && self.mute_fails {
            Err("mock unmute failure".into())
        } else {
            self.fake.set_device_muted(id, kind, value)
        }
    }
    fn set_app_volume(&self, id: &str, value: f32) -> Result<(), String> {
        self.fake.set_app_volume(id, value)
    }
    fn set_app_muted(&self, id: &str, value: bool) -> Result<(), String> {
        self.fake.set_app_muted(id, value)
    }
    fn subscribe(&self) -> AudioSubscription {
        self.fake.subscribe()
    }
}

#[tokio::test]
async fn failed_unmute_or_volume_does_not_short_circuit_other_members() {
    for mute_fails in [false, true] {
        let mut bad = FakeAudio::device("bad", AudioDeviceKind::Output, true, 0.7);
        bad.muted = mute_fails;
        let fake = Arc::new(FakeAudio::new(
            vec![
                bad,
                FakeAudio::device("healthy", AudioDeviceKind::Output, false, 0.7),
            ],
            vec![],
        ));
        let audio = setup(
            Arc::new(FailingAudio {
                fake: fake.clone(),
                mute_fails,
                volume_fails: !mute_fails,
            }),
            vec![member("bad"), member("healthy")],
        );
        assert!(audio
            .start_group_volume("g", 0.25, true)
            .unwrap()
            .await
            .unwrap()
            .is_err());
        assert_eq!(
            fake.writes(),
            vec![FakeWrite::Volume("healthy".into(), 0.25)]
        );
        let report = audio.group_report("g").unwrap();
        let op = report.operation.unwrap();
        assert_eq!(
            op.members[0].outcome,
            if mute_fails {
                AudioGroupOutcome::Failed
            } else {
                AudioGroupOutcome::Unconfirmed
            }
        );
        assert_eq!(op.members[1].outcome, AudioGroupOutcome::Confirmed);
    }
}

#[tokio::test]
async fn unsupported_group_operations_never_write_and_reports_are_observational() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = setup(fake.clone(), vec![member("out")]);
    for mode in [
        SetAudioMode::AdjustVolume,
        SetAudioMode::Mute,
        SetAudioMode::Unmute,
        SetAudioMode::ToggleMute,
        SetAudioMode::HoldMute,
    ] {
        assert!(audio
            .apply(target(), mode, 50.0)
            .await
            .unwrap_err()
            .contains("unsupported"));
    }
    assert!(audio
        .hold_press(("p".into(), 0, 0), None, target())
        .await
        .unwrap_err()
        .contains("unsupported"));
    assert!(audio
        .hold_release(("p".into(), 0, 0), None, true, target())
        .await
        .unwrap_err()
        .contains("unsupported"));
    let first = audio.group_report("g").unwrap();
    fake.change(|devices, _| devices[0].volume = 0.2);
    let second = audio.refresh_groups().remove(0);
    assert!(second.revision > first.revision);
    assert_eq!(second.members[0].volume, Some(f64::from(0.2_f32)));
    assert!(!second.restore_owned && !second.busy);
    assert!(second.operation.is_none());
    assert!(fake.writes().is_empty());
    assert_eq!(audio.jobs.len(), 0);
}

#[test]
fn sparse_wave_link_values_are_not_zero_or_false_and_read_only_levels_stay_readable() {
    use crate::wavelink::{WaveLinkControl, WaveLinkState, WaveLinkStatus, WaveLinkTarget};
    let member = AudioMemberTarget::WaveLink {
        target: WaveLinkTarget::Channel {
            channel_id: "channel".into(),
        },
    };
    let AudioMemberTarget::WaveLink { target } = &member else {
        unreachable!()
    };
    let mut wave = WaveLinkState {
        status: WaveLinkStatus::Ready,
        generation: 4,
        controls: vec![WaveLinkControl {
            target: target.clone(),
            name: "Sparse".into(),
            level: None,
            muted: None,
            can_set_level: false,
            can_mute: false,
            reason: Some("read-only".into()),
            gain: None,
        }],
        ..Default::default()
    };
    let sparse = observe(&member, &SystemAudioState::default(), Some(&wave), None);
    assert_eq!(sparse.report.volume, None);
    assert_eq!(sparse.report.muted, None);
    wave.controls[0].level = Some(0.73);
    let readable = observe(&member, &SystemAudioState::default(), Some(&wave), None);
    assert_eq!(readable.report.volume, Some(0.73));
    assert!(!readable.report.can_set_volume);
    assert_eq!(aggregate_group(&[readable.report]).volume, Some(0.73));
}

#[tokio::test]
async fn group_edits_invalidate_queued_operation_without_writes() {
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = setup(fake.clone(), vec![member("out")]);
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    audio.configure_groups(&[group(vec![member("different")])]);
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert!(response.await.unwrap().is_err());
    assert!(fake.writes().is_empty());
    assert!(audio.group_report("g").unwrap().operation.is_none());
}

#[tokio::test]
async fn read_only_group_feedback_renders_mean_and_all_known_muted_zero_without_writes() {
    use crate::profile::{Fader, LinkKind, LinkProvider, StateLink};
    let mut device = FakeAudio::device("out", AudioDeviceKind::Output, true, 0.5);
    device.can_set_volume = false;
    device.can_mute = false;
    let fake = Arc::new(FakeAudio::new(vec![device], vec![]));
    let audio = setup(fake.clone(), vec![member("out")]);
    let fader = Fader {
        min: 10.0,
        max: 110.0,
        bound: Some(target()),
        ..Fader::default()
    };
    assert_eq!(fader.render_value(&audio.live.lock()), Some(60.0));
    assert!(audio
        .start_group_volume("g", 0.9, true)
        .unwrap()
        .await
        .unwrap()
        .is_err());
    fake.change(|devices, _| devices[0].muted = true);
    audio.refresh_groups();
    assert_eq!(fader.render_value(&audio.live.lock()), Some(10.0));
    let link = StateLink {
        provider: LinkProvider::System,
        kind: LinkKind::AudioMuted,
        scene: String::new(),
        source: String::new(),
        target: Some(target()),
        wave_link_mix_id: None,
    };
    assert!(link.is_active(&audio.live.lock()));
    assert!(fake.writes().is_empty());
}

#[cfg(unix)]
fn fine_member() -> AudioMemberTarget {
    AudioMemberTarget::FineTune {
        target: crate::finetune::FineTuneTarget::App {
            app_id: "app".into(),
        },
    }
}

#[cfg(unix)]
#[tokio::test]
async fn managed_inactive_fine_tune_volume_uses_protocol_readback_and_permission_denial_is_read_only(
) {
    use crate::finetune::tests::{until, Fixture};
    use serde_json::json;
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready_managed(&fine).await;
    let audio = SystemAudio::with_integrations(
        Arc::new(NoAudio),
        SharedLive::default(),
        None,
        Some(fine.clone()),
    );
    audio.configure_groups(&[group(vec![fine_member()])]);
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setAppVolume");
    assert_eq!(setter["params"], json!({"appId":"app","volume":0.25}));
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Pending
    );
    peer.reply(&setter, json!({"accepted":true})).await;
    let getter = peer.request().await;
    assert_eq!(getter["method"], "getState");
    peer.snapshot["managedApps"][0]["volume"] = json!(0.25);
    peer.revision += 1;
    peer.snapshot_reply(&getter).await;
    response.await.unwrap().unwrap();
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Confirmed
    );
    peer.snapshot["permission"]["audioCapture"] = json!("denied");
    peer.revision += 1;
    peer.event().await;
    until(&fine, |s| s.writable_app("app").is_err()).await;
    let report = audio.refresh_groups().remove(0);
    assert_eq!(report.members[0].volume, Some(0.25));
    assert!(!report.members[0].can_set_volume);
    assert!(audio
        .start_group_volume("g", 0.8, false)
        .unwrap()
        .await
        .unwrap()
        .is_err());
    assert_eq!(audio.jobs.len(), 0);
    let device = observe(
        &AudioMemberTarget::FineTune {
            target: crate::finetune::FineTuneTarget::Output {
                device_uid: "out".into(),
            },
        },
        &SystemAudioState::default(),
        None,
        Some(&fine.state()),
    );
    assert_eq!(device.report.volume, Some(0.75));
    assert_eq!(device.report.muted, Some(false));
    assert!(!device.report.can_set_volume && !device.report.can_mute);
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[cfg(unix)]
#[tokio::test]
async fn fine_tune_unready_admission_and_new_generation_do_not_replay() {
    use crate::finetune::{
        tests::{until, Fixture},
        FineTuneStatus,
    };
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.accept().await;
    peer.hello().await;
    let subscribe = peer.request().await;
    until(&fine, |s| s.status == FineTuneStatus::Synchronizing).await;
    let audio = SystemAudio::with_integrations(
        Arc::new(NoAudio),
        SharedLive::default(),
        None,
        Some(fine.clone()),
    );
    audio.configure_groups(&[group(vec![fine_member()])]);
    let (release, blocked) = barrier(&audio).await;
    assert!(audio
        .start_group_volume("g", 0.25, false)
        .unwrap()
        .await
        .unwrap()
        .is_err());
    assert_eq!(audio.jobs.len(), 0);
    peer.snapshot_reply(&subscribe).await;
    until(&fine, |s| s.status == FineTuneStatus::Ready).await;
    let admitted = audio.start_group_volume("g", 0.35, false).unwrap();
    fine.disconnect();
    peer.closed().await;
    fine.connect();
    let mut peer = fixture.ready_session(&fine, "replacement").await;
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert!(admitted.await.unwrap().is_err());
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Skipped
    );
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_fine_tune_pid_change_before_execution_is_not_retargeted() {
    use crate::finetune::tests::{until, Fixture};
    use serde_json::json;
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready(&fine).await;
    let audio = SystemAudio::with_integrations(
        Arc::new(NoAudio),
        SharedLive::default(),
        None,
        Some(fine.clone()),
    );
    audio.configure_groups(&[group(vec![fine_member()])]);
    let (release, blocked) = barrier(&audio).await;
    let admitted = audio.start_group_volume("g", 0.35, false).unwrap();
    peer.snapshot["apps"][0]["pids"] = json!([456]);
    peer.revision += 1;
    peer.event().await;
    until(&fine, |s| s.app("app").is_ok_and(|app| app.pids == [456])).await;
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert!(admitted.await.unwrap().is_err());
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Skipped
    );
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[cfg(unix)]
#[tokio::test]
async fn unmute_ack_without_confirmed_unmute_skips_volume_but_not_healthy_members() {
    use crate::finetune::tests::{until, Fixture};
    use serde_json::json;
    let fixture = Fixture::new().await;
    let fine = fixture.start(true);
    let mut peer = fixture.ready_managed(&fine).await;
    peer.snapshot["managedApps"][0]["muted"] = json!(true);
    peer.revision += 1;
    peer.event().await;
    until(&fine, |state| state.app("app").is_ok_and(|app| app.muted)).await;
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio = SystemAudio::with_integrations(
        fake.clone(),
        SharedLive::default(),
        None,
        Some(fine.clone()),
    );
    audio.configure_groups(&[group(vec![fine_member(), member("out")])]);
    let response = audio.start_group_volume("g", 0.25, true).unwrap();
    let unmute = peer.request().await;
    assert_eq!(unmute["method"], "setAppMute");
    assert_eq!(unmute["params"], json!({"appId":"app","muted":false}));
    peer.reply(&unmute, json!({"accepted":true})).await;
    let getter = peer.request().await;
    assert_eq!(getter["method"], "getState");
    peer.revision += 1;
    peer.snapshot_reply(&getter).await;
    assert!(response
        .await
        .unwrap()
        .unwrap_err()
        .contains("volume not written"));
    assert_eq!(fake.writes(), vec![FakeWrite::Volume("out".into(), 0.25)]);
    let report = audio.group_report("g").unwrap();
    assert_eq!(
        report.operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Unconfirmed
    );
    fine.refresh();
    peer.getter().await;
    fine.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn wave_link_failed_rpc_does_not_block_healthy_native_member() {
    use crate::wavelink::{
        tests::{catalog, until, Fixture},
        WaveLinkStatus,
    };
    use serde_json::json;
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let fake = Arc::new(FakeAudio::new(
        vec![FakeAudio::device("out", AudioDeviceKind::Output, true, 0.7)],
        vec![],
    ));
    let audio =
        SystemAudio::with_wave_link(fake.clone(), SharedLive::default(), Some(wave.clone()));
    audio.configure_groups(&[group(vec![
        AudioMemberTarget::WaveLink {
            target: crate::wavelink::WaveLinkTarget::Channel {
                channel_id: "channel".into(),
            },
        },
        member("out"),
    ])]);
    let response = audio.start_group_volume("g", 0.25, false).unwrap();
    let setter = peer.request().await;
    assert_eq!(setter["method"], "setChannel");
    peer.send(json!({"jsonrpc":"2.0","id":setter["id"],"error":{"code":-32601,"message":"fixture unsupported"}})).await;
    assert!(response.await.unwrap().is_err());
    assert_eq!(fake.writes(), vec![FakeWrite::Volume("out".into(), 0.25)]);
    let report = audio.group_report("g").unwrap();
    let outcomes = report.operation.unwrap().members;
    assert_eq!(outcomes[0].outcome, AudioGroupOutcome::Unconfirmed);
    assert_eq!(outcomes[1].outcome, AudioGroupOutcome::Confirmed);
    let resync = peer.request().await;
    assert_eq!(resync["method"], "getChannels");
    let synchronizing = until(&wave, |s| s.status == WaveLinkStatus::Synchronizing).await;
    let (release, blocked) = barrier(&audio).await;
    let response = audio.start_group_volume("g", 0.4, false).unwrap();
    assert_eq!(
        audio.group_report("g").unwrap().operation.unwrap().members[0].outcome,
        AudioGroupOutcome::Skipped
    );
    peer.reply(&resync, catalog("getChannels")).await;
    let ready = until(&wave, |s| s.status == WaveLinkStatus::Ready).await;
    assert_eq!(ready.generation, synchronizing.generation);
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    response.await.unwrap().unwrap();
    assert_eq!(
        fake.writes(),
        vec![
            FakeWrite::Volume("out".into(), 0.25),
            FakeWrite::Volume("out".into(), 0.4)
        ]
    );
    wave.refresh();
    for _ in 0..4 {
        peer.getter().await;
    }
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn wave_link_unready_group_admission_is_not_queued_for_same_generation_ready() {
    use crate::wavelink::{
        tests::{catalog, until, Fixture},
        WaveLinkStatus,
    };
    use serde_json::json;
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.accept().await;
    let identify = peer.request().await;
    peer.reply(
        &identify,
        json!({"name":"fixture","operatingSystem":"mac","appID":"EWL","interfaceRevision":2}),
    )
    .await;
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(peer.request().await);
    }
    let state = until(&wave, |s| s.status == WaveLinkStatus::Synchronizing).await;
    let audio =
        SystemAudio::with_wave_link(Arc::new(NoAudio), SharedLive::default(), Some(wave.clone()));
    audio.configure_groups(&[group(vec![AudioMemberTarget::WaveLink {
        target: crate::wavelink::WaveLinkTarget::Channel {
            channel_id: "channel".into(),
        },
    }])]);
    let (release, blocked) = barrier(&audio).await;
    assert!(audio
        .start_group_volume("g", 0.25, false)
        .unwrap()
        .await
        .unwrap()
        .is_err());
    assert_eq!(audio.jobs.len(), 0);
    for request in held {
        peer.reply(&request, catalog(request["method"].as_str().unwrap()))
            .await;
    }
    let ready = until(&wave, |s| s.status == WaveLinkStatus::Ready).await;
    assert_eq!(state.generation, ready.generation);
    release.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    audio.run_writer(|_| Ok(())).await.unwrap();
    for _ in 0..4 {
        peer.getter().await;
    }
    wave.shutdown();
    peer.closed().await;
}

#[tokio::test]
async fn wave_link_group_confirmation_requires_actual_level_readback_not_rpc_ack() {
    use crate::wavelink::tests::{catalog, Fixture};
    use serde_json::json;
    let fixture = Fixture::new().await;
    let wave = fixture.start(true);
    let mut peer = fixture.ready(&wave).await;
    let audio =
        SystemAudio::with_wave_link(Arc::new(NoAudio), SharedLive::default(), Some(wave.clone()));
    audio.configure_groups(&[group(vec![AudioMemberTarget::WaveLink {
        target: crate::wavelink::WaveLinkTarget::Channel {
            channel_id: "channel".into(),
        },
    }])]);
    for confirmed in [false, true] {
        let response = audio.start_group_volume("g", 0.25, false).unwrap();
        let setter = peer.request().await;
        assert_eq!(setter["method"], "setChannel");
        assert_eq!(setter["params"], json!({"id":"channel","level":0.25}));
        peer.reply(&setter, json!({})).await;
        let getter = peer.request().await;
        assert_eq!(getter["method"], "getChannels");
        let mut snapshot = catalog("getChannels");
        if confirmed {
            snapshot["channels"][0]["level"] = json!(0.25);
        }
        peer.reply(&getter, snapshot).await;
        assert_eq!(response.await.unwrap().is_ok(), confirmed);
        let report = audio.group_report("g").unwrap();
        assert_eq!(
            report.members[0].volume,
            Some(if confirmed { 0.25 } else { 0.7 })
        );
        assert_eq!(
            report.operation.unwrap().members[0].outcome,
            if confirmed {
                AudioGroupOutcome::Confirmed
            } else {
                AudioGroupOutcome::Unconfirmed
            }
        );
    }
    wave.shutdown();
    peer.closed().await;
}
