//! LED renderer: paints the active page on the device.
//!
//! One render thread per connection. Anyone can ask for a repaint by sending
//! [`RenderCmd::Repaint`]; requests are coalesced and only LEDs whose colour
//! changed since the last frame are sent. [`RenderCmd::Reset`] forgets the
//! last frame (used after the wake-up sweep or a manual clear) so the next
//! paint is complete.

use super::manager::OutputHandle;
use super::types::*;
use crate::live::{LiveStatus, SharedLive};
use crate::profile::ProfileStore;
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Colour for a held pad that has no button (connection check).
pub const PRESS_COLOR: Color = Color::new(255, 122, 26);
/// Legacy lit the logo LED of X / Pro MK3 as a soft "app is driving me" hint.
const LOGO_COLOR: LedColor = LedColor::Pulsing(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderCmd {
    Repaint,
    Reset,
    Close,
}

/// Everything the renderer reads to build a frame.
#[derive(Clone)]
pub struct RenderInputs {
    pub profile: Arc<Mutex<ProfileStore>>,
    pub pressed: Arc<Mutex<HashSet<(u8, u8)>>>,
    /// (page, x, y) of every running macro
    pub running: Arc<Mutex<HashSet<(String, u8, u8)>>>,
    pub press_feedback: Arc<Mutex<bool>>,
    /// Streaming app state for buttons with a `state_link`.
    pub live: SharedLive,
    /// Bumped after every frame the renderer builds, so others showing the pads
    /// (the phone remote) can follow without a renderer of their own.
    pub ticks: FrameTicks,
}

/// Counter bumped whenever the pads were (re)painted or the connection changed.
pub type FrameTicks = Arc<tokio::sync::watch::Sender<u64>>;

pub fn frame_ticks() -> FrameTicks {
    Arc::new(tokio::sync::watch::channel(0).0)
}

pub fn bump(ticks: &FrameTicks) {
    ticks.send_modify(|n| *n = n.wrapping_add(1));
}

#[derive(Clone)]
pub struct RenderHandle {
    tx: Sender<RenderCmd>,
}

impl RenderHandle {
    pub fn repaint(&self) {
        let _ = self.tx.send(RenderCmd::Repaint);
    }
    pub fn reset(&self) {
        let _ = self.tx.send(RenderCmd::Reset);
    }
    pub fn close(&self) {
        let _ = self.tx.send(RenderCmd::Close);
    }
}

pub fn spawn_renderer(output: OutputHandle, layout: Layout, inputs: RenderInputs) -> RenderHandle {
    let (tx, rx) = unbounded::<RenderCmd>();
    std::thread::Builder::new()
        .name("lunchpad-led-render".into())
        .spawn(move || render_loop(rx, output, layout, inputs))
        .expect("failed to spawn LED render thread");
    RenderHandle { tx }
}

fn render_loop(rx: Receiver<RenderCmd>, output: OutputHandle, layout: Layout, inputs: RenderInputs) {
    let mut last: HashMap<(u8, u8), LedColor> = HashMap::new();
    while let Ok(mut cmd) = rx.recv() {
        // Coalesce a burst of requests into one frame.
        while let Ok(next) = rx.try_recv() {
            cmd = match (cmd, next) {
                (RenderCmd::Close, _) | (_, RenderCmd::Close) => RenderCmd::Close,
                (RenderCmd::Reset, _) | (_, RenderCmd::Reset) => RenderCmd::Reset,
                _ => RenderCmd::Repaint,
            };
        }
        match cmd {
            RenderCmd::Close => break,
            RenderCmd::Reset => last.clear(),
            RenderCmd::Repaint => {}
        }
        let frame = build_frame(&layout, &inputs);
        bump(&inputs.ticks);
        let changed: Vec<(u8, u8, LedColor)> = frame
            .iter()
            .filter(|((x, y), c)| last.get(&(*x, *y)) != Some(*c))
            .map(|((x, y), c)| (*x, *y, *c))
            .collect();
        if changed.is_empty() {
            continue;
        }
        tracing::trace!(leds = changed.len(), "painting");
        if output.set_leds(&changed).is_err() {
            break;
        }
        last = frame;
    }
    tracing::debug!("LED render thread finished");
}

/// Compute the colour of every LED for the current profile state.
pub fn build_frame(layout: &Layout, inputs: &RenderInputs) -> HashMap<(u8, u8), LedColor> {
    let profile = inputs.profile.lock();
    let page = profile.profile.active();
    let pressed = inputs.pressed.lock().clone();
    // Only macros of the page on the device light their pad.
    let running: HashSet<(u8, u8)> = {
        let all = inputs.running.lock();
        let active_id = page.map(|p| p.id.as_str());
        all.iter().filter(|(p, _, _)| Some(p.as_str()) == active_id).map(|(_, x, y)| (*x, *y)).collect()
    };
    let feedback = *inputs.press_feedback.lock();
    let live: LiveStatus = inputs.live.lock().clone();

    let mut frame = HashMap::with_capacity(layout.pads.len());
    for pad in &layout.pads {
        if pad.led == LedKind::None {
            continue;
        }
        let color = match pad.shape {
            PadShape::Empty => continue,
            PadShape::Logo => LOGO_COLOR,
            _ => {
                if let Some((fader, step)) = page.and_then(|p| p.fader_at(pad.x, pad.y)) {
                    let [r, g, b] = fader.render_rgb(step, &live);
                    frame.insert((pad.x, pad.y), if r == 0 && g == 0 && b == 0 { LedColor::Off } else { LedColor::Rgb(Color::new(r, g, b)) });
                    continue;
                }
                let active = pressed.contains(&(pad.x, pad.y)) || running.contains(&(pad.x, pad.y));
                match page.and_then(|p| p.get(pad.x, pad.y)) {
                    Some(button) => {
                        let linked = button.state_link.as_ref().map(|l| l.is_active(&live)).unwrap_or(false);
                        let c = if active || linked { button.active_color.unwrap_or(button.color) } else { button.color };
                        pad_color_to_led(c)
                    }
                    None if active && feedback => LedColor::Rgb(PRESS_COLOR),
                    None => LedColor::Off,
                }
            }
        };
        frame.insert((pad.x, pad.y), color);
    }
    frame
}

fn pad_color_to_led(c: crate::profile::PadColor) -> LedColor {
    use crate::profile::PadColor;
    match c {
        PadColor::Palette { index } => {
            if index == 0 {
                LedColor::Off
            } else {
                LedColor::Palette(index)
            }
        }
        PadColor::Flashing { index, alt } => LedColor::Flashing { index, alt },
        PadColor::Pulsing { index } => LedColor::Pulsing(index),
        PadColor::Rgb { r, g, b } => {
            if r == 0 && g == 0 && b == 0 {
                LedColor::Off
            } else {
                LedColor::Rgb(Color::new(r, g, b))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::models::driver_for;
    use crate::profile::{Button, PadColor};

    fn inputs() -> RenderInputs {
        let dir = std::env::temp_dir().join(format!("lunchpad-render-{}", uuid::Uuid::new_v4()));
        RenderInputs {
            profile: Arc::new(Mutex::new(ProfileStore::load(&dir))),
            pressed: Arc::new(Mutex::new(HashSet::new())),
            running: Arc::new(Mutex::new(HashSet::new())),
            press_feedback: Arc::new(Mutex::new(true)),
            live: Default::default(),
            ticks: frame_ticks(),
        }
    }

    #[test]
    fn frame_uses_active_color_while_pressed() {
        let inputs = inputs();
        {
            let mut store = inputs.profile.lock();
            let page = store.profile.pages.first_mut().unwrap();
            page.set(0, 0, Button { color: PadColor::Palette { index: 5 }, active_color: Some(PadColor::Palette { index: 21 }), ..Default::default() });
        }
        let layout = driver_for(LaunchpadModel::LaunchpadMk2).layout();
        let frame = build_frame(&layout, &inputs);
        assert_eq!(frame[&(0, 0)], LedColor::Palette(5));
        assert_eq!(frame[&(1, 0)], LedColor::Off);
        assert!(!frame.contains_key(&(8, 8)), "empty corner has no LED");

        inputs.pressed.lock().insert((0, 0));
        inputs.pressed.lock().insert((1, 0));
        let frame = build_frame(&layout, &inputs);
        assert_eq!(frame[&(0, 0)], LedColor::Palette(21));
        assert_eq!(frame[&(1, 0)], LedColor::Rgb(PRESS_COLOR), "held empty pad shows press feedback");
    }

    #[test]
    fn a_bound_fader_shows_muted_as_empty_and_missing_as_unavailable() {
        use crate::profile::{AudioDeviceKind, AudioTarget, DeviceRef, Fader};
        use crate::system_audio::{FakeAudio, SystemAudioState};
        let inputs = inputs();
        let fader = Fader { id: "mic".into(), x: 0, y: 0, length: 4, min: 0.0, max: 100.0, value: 100.0, dim: 0, bound: Some(AudioTarget::Device { device: DeviceRef::DefaultInput }), ..Fader::default() };
        inputs.profile.lock().profile.pages.first_mut().unwrap().set_fader(fader.clone());
        let layout = driver_for(LaunchpadModel::LaunchpadMk2).layout();
        let lit = |frame: &HashMap<(u8, u8), LedColor>| (0..4u8).filter(|y| frame[&(0, *y)] != LedColor::Off).count();

        // No system audio: unavailable, only the first pad (dimmed; 0 % here = off).
        assert_eq!(lit(&build_frame(&layout, &inputs)), 0);
        let mut mic = FakeAudio::device("m", AudioDeviceKind::Input, true, 1.0);
        inputs.live.lock().system = SystemAudioState { available: true, devices: vec![mic.clone()], ..Default::default() };
        assert_eq!(lit(&build_frame(&layout, &inputs)), 4, "full level");
        mic.muted = true;
        inputs.live.lock().system.devices = vec![mic];
        assert_eq!(lit(&build_frame(&layout, &inputs)), 1, "muted shows the minimum");
        assert_eq!(inputs.profile.lock().profile.pages[0].faders[0].value, 100.0, "the value is kept");

        // Unavailable with a visible dim: the first pad only, at the dim level.
        let dimmed = Fader { dim: 50, ..fader };
        let empty = crate::live::LiveStatus::default();
        assert_eq!(dimmed.render_value(&empty), None);
        assert_eq!(dimmed.render_rgb(0, &empty), [0, 48, 128]);
        assert_eq!(dimmed.render_rgb(1, &empty), [0, 0, 0]);
        // An unbound fader ignores the system audio.
        let plain = Fader { bound: None, value: 30.0, ..dimmed };
        assert_eq!(plain.render_value(&empty), Some(30.0));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_fine_tune_app_renders_observed_state_in_both_muted_modes() {
        use crate::finetune::{FineTuneCapturePermission, FineTuneSnapshot, FineTuneState, FineTuneStatus, FineTuneTarget};
        use crate::profile::{AudioTarget, Fader};
        let inputs = inputs();
        let layout = driver_for(LaunchpadModel::LaunchpadMk2).layout();
        let mut snapshot: FineTuneSnapshot = serde_json::from_value(crate::finetune::tests::snapshot()).unwrap();
        snapshot.permission.audio_capture = FineTuneCapturePermission::Denied;
        inputs.live.lock().fine_tune = FineTuneState { status: FineTuneStatus::Ready, snapshot: Some(snapshot), ..Default::default() };
        let target = AudioTarget::FineTune { target: FineTuneTarget::App { app_id: "app".into() } };
        {
            let live = inputs.live.lock();
            assert!(live.fine_tune.writable_app("app").is_err(), "writes stay rejected");
            assert!(!live.resolve_audio(&target).unwrap().can_set_volume, "fader presses stay rejected");
        }
        let base = Fader { id: "ft".into(), x: 0, y: 0, length: 5, min: 0.0, max: 100.0, value: 50.0, dim: 25, muted_color: [200, 80, 40], bound: Some(target), ..Fader::default() };
        for muted_level in [false, true] {
            let fader = Fader { muted_level, ..base.clone() };
            inputs.profile.lock().profile.pages[0].faders = vec![fader.clone()];
            inputs.live.lock().fine_tune.snapshot.as_mut().unwrap().apps[0].muted = false;
            let frame = build_frame(&layout, &inputs);
            for (step, pad) in fader.pads().iter().enumerate() {
                let [r, g, b] = fader.pad_rgb(step);
                assert_eq!(frame[pad], LedColor::Rgb(Color::new(r, g, b)), "unmuted shows the stored level, not unavailable");
            }
            inputs.live.lock().fine_tune.snapshot.as_mut().unwrap().apps[0].muted = true;
            let frame = build_frame(&layout, &inputs);
            let zero = Fader { value: 0.0, ..fader.clone() };
            for (step, pad) in fader.pads().iter().enumerate() {
                let expected = if !muted_level {
                    let [r, g, b] = zero.pad_rgb(step);
                    if r == 0 && g == 0 && b == 0 { LedColor::Off } else { LedColor::Rgb(Color::new(r, g, b)) }
                } else if step <= 2 {
                    LedColor::Rgb(Color::new(200, 80, 40))
                } else {
                    LedColor::Rgb(Color::new(50, 20, 10))
                };
                assert_eq!(frame[pad], expected, "muted_level={muted_level} step={step}");
            }
            assert_eq!(inputs.profile.lock().profile.pages[0].faders[0].value, 50.0);
        }
    }

    #[test]
    fn wave_link_control_without_a_level_is_unavailable_not_zero() {
        use crate::profile::{AudioTarget, Fader};
        use crate::wavelink::{WaveLinkControl, WaveLinkState, WaveLinkStatus, WaveLinkTarget};
        let target = WaveLinkTarget::Channel { channel_id: "c".into() };
        let control = WaveLinkControl { target: target.clone(), name: "C".into(), level: None, muted: Some(true), can_set_level: true, can_mute: true, reason: None, gain: None };
        let mut live = crate::live::LiveStatus { wave_link: WaveLinkState { status: WaveLinkStatus::Ready, controls: vec![control], ..Default::default() }, ..Default::default() };
        let fader = Fader { value: 60.0, muted_level: true, bound: Some(AudioTarget::WaveLink { target }), ..Fader::default() };
        assert_eq!(fader.render_value(&live), None);
        live.wave_link.controls[0].level = Some(0.6);
        assert_eq!(fader.render_value(&live), Some(60.0));
    }

    #[test]
    fn muted_fader_frames_use_retained_level_and_custom_color_in_every_direction() {
        use crate::profile::{AudioDeviceKind, AudioTarget, DeviceRef, Fader, FaderDirection};
        use crate::system_audio::{FakeAudio, SystemAudioState};
        let inputs = inputs();
        let layout = driver_for(LaunchpadModel::LaunchpadMk2).layout();
        let mut mic = FakeAudio::device("m", AudioDeviceKind::Input, true, 0.1);
        mic.muted = true;
        inputs.live.lock().system = SystemAudioState { available: true, devices: vec![mic], ..Default::default() };
        for direction in [FaderDirection::Up, FaderDirection::Down, FaderDirection::Left, FaderDirection::Right] {
            for (min, max, value) in [(-60.0, 20.0, -20.0), (20.0, -60.0, -20.0)] {
                let fader = Fader { id: "muted".into(), x: 3, y: 3, length: 5, direction, min, max, value, dim: 25, muted_level: true, muted_color: [200, 80, 40], bound: Some(AudioTarget::Device { device: DeviceRef::DefaultInput }), ..Fader::default() };
                inputs.profile.lock().profile.pages[0].faders = vec![fader.clone()];
                let frame = build_frame(&layout, &inputs);
                for (step, pad) in fader.pads().iter().enumerate() {
                    assert_eq!(frame[pad], LedColor::Rgb(if step <= 2 { Color::new(200, 80, 40) } else { Color::new(50, 20, 10) }));
                }
                let ordinary = Fader { muted_level: false, ..fader.clone() };
                inputs.profile.lock().profile.pages[0].faders = vec![ordinary.clone()];
                let default_frame = build_frame(&layout, &inputs);
                let zero = Fader { value: min, ..ordinary.clone() };
                for (step, pad) in fader.pads().iter().enumerate() {
                    let [r, g, b] = zero.pad_rgb(step);
                    assert_eq!(default_frame[pad], LedColor::Rgb(Color::new(r, g, b)));
                }
                inputs.live.lock().system.devices[0].muted = false;
                let unmuted = inputs.live.lock().clone();
                for step in 0..5 { assert_eq!(fader.render_rgb(step, &unmuted), ordinary.render_rgb(step, &unmuted)); }
                inputs.live.lock().system.devices[0].muted = true;
                let unavailable = crate::live::LiveStatus::default();
                for step in 0..5 { assert_eq!(fader.render_rgb(step, &unavailable), ordinary.render_rgb(step, &unavailable)); }
                assert_eq!(inputs.profile.lock().profile.pages[0].faders[0].value, value);
            }
        }
    }
}
