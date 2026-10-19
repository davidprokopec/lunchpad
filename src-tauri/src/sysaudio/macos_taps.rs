//! Per-app volume and mute through Core Audio process taps (macOS 14.2+).
//! Only reachable with the `audio.macosAppVolume` setting (off by default).
//!
//! UNVERIFIED: this path compiles but has never run (ported from LaunchDeck,
//! where it was never ear-verified either). It needs the System Audio
//! Recording permission, which a bundled `.app` with
//! `NSAudioCaptureUsageDescription` can obtain. Verify with the user present
//! before enabling it.
//!
//! Per app: a private tap over all of its process objects (muted, so the app's
//! own output is silenced), a private aggregate device on the default output,
//! and an IOProc that re-outputs the tapped audio times the gain.
//!
//! Permission safety: denial is silent (zeros, no error),
//! and a muted tap would then silence the app. Before the first muted tap we
//! run a probe: an UNMUTED tap whose IOProc outputs nothing and measures the
//! input peak while the app is producing audio. Only a non-zero peak counts as
//! evidence of the permission; silence is reported as unconfirmed, never as
//! granted or denied. After that, muted taps fail open (G1, `SilenceWatch`)
//! and follow the default output (G2, `route`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use super::engine::{Error, ErrorKind, Permission};
use super::macos::{
    check, get, get_string, CaResult, MacBackend, OSStatus, ObjId, DEV_UID, HW_DEFAULT_OUTPUT,
    SYSTEM_OBJECT, TAP_FORMAT,
};

type CFRef = *const c_void;

const PROBE_WINDOW: Duration = Duration::from_millis(400);
const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_FLAG_IS_FLOAT: u32 = 1;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    byte_size: u32,
    data: *mut c_void,
}

/// Variable-length in C: `count` buffers follow.
#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

/// # Safety
/// `p` must be null or point to a valid AudioBufferList for the lifetime `'a`.
unsafe fn buffers<'a>(p: *const AudioBufferList) -> &'a [AudioBuffer] {
    if p.is_null() {
        return &[];
    }
    // SAFETY: the HAL lays out `count` contiguous AudioBuffers from `buffers`.
    unsafe { std::slice::from_raw_parts((*p).buffers.as_ptr(), (*p).count as usize) }
}

type IOProc = unsafe extern "C" fn(
    ObjId,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> OSStatus;
type IOProcId = *mut c_void;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    // Declared under __OBJC__ in AudioHardwareTapping.h but a plain C symbol.
    fn AudioHardwareCreateProcessTap(desc: *const AnyObject, out: *mut ObjId) -> OSStatus;
    fn AudioHardwareDestroyProcessTap(tap: ObjId) -> OSStatus;
    fn AudioHardwareCreateAggregateDevice(desc: CFRef, out: *mut ObjId) -> OSStatus;
    fn AudioHardwareDestroyAggregateDevice(dev: ObjId) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        dev: ObjId,
        p: IOProc,
        client: *mut c_void,
        out: *mut IOProcId,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(dev: ObjId, id: IOProcId) -> OSStatus;
    fn AudioDeviceStart(dev: ObjId, id: IOProcId) -> OSStatus;
    fn AudioDeviceStop(dev: ObjId, id: IOProcId) -> OSStatus;
}

fn native(e: super::macos::CaError) -> Error {
    e.into_error(ErrorKind::Native)
}

struct Tap {
    id: ObjId,
    uid: String,
}

impl Tap {
    /// Private stereo-mixdown tap of `processes`; `muted` silences their output.
    fn create(processes: &[ObjId], muted: bool) -> Result<Tap, Error> {
        let class = AnyClass::get(c"CATapDescription")
            .ok_or_else(|| Error::new(ErrorKind::Native, "CATapDescription is unavailable"))?;
        let numbers: Vec<Retained<NSNumber>> = processes
            .iter()
            .map(|p| NSNumber::numberWithUnsignedInt(*p))
            .collect();
        let list = NSArray::from_retained_slice(&numbers);
        // SAFETY: selectors and argument types match CATapDescription.h:
        // initStereoMixdownOfProcesses:(NSArray<NSNumber*>*), setName:(NSString*),
        // setPrivate:(BOOL), setMuteBehavior:(CATapMuteBehavior = NSInteger),
        // UUID -> NSUUID*. `alloc`/`init` follow the Allocated/Retained rules.
        let (desc, uuid): (Retained<AnyObject>, Retained<NSUUID>) = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![class, alloc];
            let desc: Retained<AnyObject> =
                msg_send![allocated, initStereoMixdownOfProcesses: &*list];
            let _: () = msg_send![&desc, setName: &*NSString::from_str("lunchpad-sysaudio")];
            let _: () = msg_send![&desc, setPrivate: true];
            let _: () = msg_send![&desc, setMuteBehavior: if muted { 1isize } else { 0isize }];
            let uuid: Retained<NSUUID> = msg_send![&desc, UUID];
            (desc, uuid)
        };
        let mut id = 0;
        // SAFETY: `desc` is a live CATapDescription; `id` is writable.
        check("AudioHardwareCreateProcessTap", unsafe {
            AudioHardwareCreateProcessTap(&*desc, &mut id)
        })
        .map_err(native)?;
        Ok(Tap {
            id,
            uid: uuid.UUIDString().to_string(),
        })
    }

    fn is_float_format(&self) -> CaResult<bool> {
        let format: Asbd = get(self.id, TAP_FORMAT)?;
        Ok(format.format_id == FORMAT_LINEAR_PCM
            && format.format_flags & FORMAT_FLAG_IS_FLOAT != 0
            && format.bits_per_channel == 32)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: `id` is a tap this struct created and has not destroyed.
        unsafe { AudioHardwareDestroyProcessTap(self.id) };
    }
}

struct Aggregate {
    id: ObjId,
}

impl Aggregate {
    /// Private aggregate: main sub-device = the output, plus the tap.
    fn create(output_uid: &str, tap_uid: &str) -> Result<Aggregate, Error> {
        let s = NSString::from_str;
        let sub = NSDictionary::from_slices(&[&*s("uid")], &[&*s(output_uid) as &AnyObject]);
        let tap = NSDictionary::from_slices(
            &[&*s("uid"), &*s("drift")],
            &[
                &*s(tap_uid) as &AnyObject,
                &*NSNumber::numberWithBool(true) as &AnyObject,
            ],
        );
        let uid = format!("lunchpad.sysaudio.{}", NSUUID::new().UUIDString());
        let subs = NSArray::from_retained_slice(&[sub]);
        let taps = NSArray::from_retained_slice(&[tap]);
        let yes = NSNumber::numberWithBool(true);
        let no = NSNumber::numberWithBool(false);
        let (name, uid, main) = (s("Lunchpad audio"), s(&uid), s(output_uid));
        let keys = [
            &*s("name"),
            &*s("uid"),
            &*s("master"),
            &*s("private"),
            &*s("stacked"),
            &*s("tapautostart"),
            &*s("subdevices"),
            &*s("taps"),
        ];
        let values: [&AnyObject; 8] = [&name, &uid, &main, &yes, &no, &yes, &subs, &taps];
        let dict = NSDictionary::from_slices(&keys, &values);
        let mut id = 0;
        // SAFETY: NSDictionary is toll-free bridged to CFDictionaryRef and
        // stays alive for the call; `id` is writable.
        check("AudioHardwareCreateAggregateDevice", unsafe {
            AudioHardwareCreateAggregateDevice(Retained::as_ptr(&dict).cast(), &mut id)
        })
        .map_err(native)?;
        Ok(Aggregate { id })
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: `id` is an aggregate this struct created and has not destroyed.
        unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
    }
}

/// Shared with the real-time IOProc: atomics only, no locks, no allocation.
struct Gain {
    gain_bits: AtomicU32,
    peak_bits: AtomicU32,
}

impl Gain {
    fn new(gain: f32) -> Gain {
        Gain {
            gain_bits: AtomicU32::new(gain.to_bits()),
            peak_bits: AtomicU32::new(0),
        }
    }
    fn set(&self, gain: f32) {
        self.gain_bits
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
    /// The loudest input since the last call.
    fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed))
    }
}

/// Copies the tap input (interleaved f32) into every output buffer times the
/// gain, and records the input peak.
///
/// # Safety
/// Called by the HAL with valid buffer lists and the `client` pointer given to
/// `AudioDeviceCreateIOProcID`, which points to a `Gain` that outlives the
/// IOProc (`Running` keeps the Arc until after `AudioDeviceDestroyIOProcID`).
unsafe extern "C" fn io_proc(
    _device: ObjId,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    output: *mut AudioBufferList,
    _output_time: *const c_void,
    client: *mut c_void,
) -> OSStatus {
    // SAFETY: see the function contract.
    let (gain_state, ins, outs) =
        unsafe { (&*(client as *const Gain), buffers(input), buffers(output)) };
    let gain = f32::from_bits(gain_state.gain_bits.load(Ordering::Relaxed));
    let input = ins.first().filter(|b| !b.data.is_null() && b.channels > 0);
    // SAFETY: the tap format was checked to be 32-bit float, so the input
    // buffer holds `byte_size / 4` f32 samples.
    let samples: &[f32] = input.map_or(&[], |b| unsafe {
        std::slice::from_raw_parts(b.data as *const f32, b.byte_size as usize / 4)
    });
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    for out in outs.iter().filter(|b| !b.data.is_null()) {
        // SAFETY: the output device is the f32 main sub-device; the HAL owns
        // the buffer for the duration of this callback.
        let o = unsafe {
            std::slice::from_raw_parts_mut(out.data as *mut f32, out.byte_size as usize / 4)
        };
        let in_channels = input.map_or(1, |b| b.channels as usize);
        let out_channels = out.channels.max(1) as usize;
        let frames = (samples.len() / in_channels).min(o.len() / out_channels);
        for f in 0..frames {
            for c in 0..out_channels {
                o[f * out_channels + c] = samples[f * in_channels + (c % in_channels)] * gain;
            }
        }
        o[frames * out_channels..].fill(0.0);
    }
    if peak > gain_state.peak() {
        gain_state
            .peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
    }
    0
}

/// A started IOProc. Must drop before its aggregate device.
struct Running {
    device: ObjId,
    id: IOProcId,
    _gain: Arc<Gain>,
}

impl Running {
    fn start(device: ObjId, gain: Arc<Gain>) -> Result<Running, Error> {
        let mut id: IOProcId = std::ptr::null_mut();
        let client = Arc::as_ptr(&gain) as *mut c_void;
        // SAFETY: `client` stays valid while `Running` holds the Arc.
        check("AudioDeviceCreateIOProcID", unsafe {
            AudioDeviceCreateIOProcID(device, io_proc, client, &mut id)
        })
        .map_err(native)?;
        let running = Running {
            device,
            id,
            _gain: gain,
        };
        // SAFETY: `id` was just created on `device`.
        check("AudioDeviceStart", unsafe { AudioDeviceStart(device, id) }).map_err(native)?;
        Ok(running)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: stop then destroy the IOProc this struct created; after
        // destroy the HAL no longer calls it, so the Arc may be released.
        unsafe {
            AudioDeviceStop(self.device, self.id);
            AudioDeviceDestroyIOProcID(self.device, self.id);
        }
    }
}

/// The output device a control plays to. The object id is kept as well as the
/// UID: a device unplugged and plugged back gets a new object, and the old
/// aggregate no longer reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Output {
    object: ObjId,
    uid: String,
}

fn default_output() -> Result<Output, Error> {
    let object: ObjId = get(SYSTEM_OBJECT, HW_DEFAULT_OUTPUT).map_err(native)?;
    if object == 0 {
        return Err(Error::new(
            ErrorKind::NotFound,
            "there is no default output device",
        ));
    }
    let uid = get_string(object, DEV_UID)
        .map_err(native)?
        .ok_or_else(|| Error::new(ErrorKind::Native, "the default output has no UID"))?;
    Ok(Output { object, uid })
}

/// G2: what a control does when the default output is checked again.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Keep,
    /// The default output changed: rebuild the tap's aggregate on it.
    Rebuild(Output),
    /// No default output any more: drop the tap, which restores the app's
    /// direct path (fail open).
    Drop,
}

fn route(current: &Output, now: Option<&Output>) -> Route {
    match now {
        Some(now) if now == current => Route::Keep,
        Some(now) => Route::Rebuild(now.clone()),
        None => Route::Drop,
    }
}

/// G1: muted taps fail open. A muted tap without the System Audio Recording
/// permission delivers zeros while the app stays muted, so the app would go
/// silent while Lunchpad reports success. If the tap's input stays silent for
/// `SILENCE_LIMIT` while the app is producing output, the tap is dropped.
const SILENCE_LIMIT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct SilenceWatch {
    since: Option<Instant>,
}

impl SilenceWatch {
    /// `producing`: the app is running output now. `heard`: the tap delivered
    /// a non-zero sample since the last observation. True = drop the tap.
    fn observe(&mut self, now: Instant, producing: bool, heard: bool) -> bool {
        if heard || !producing {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= SILENCE_LIMIT
    }
}

/// One tapped app. Field order is drop order: IOProc, aggregate, tap.
struct Control {
    _running: Running,
    _aggregate: Aggregate,
    _tap: Tap,
    gain: Arc<Gain>,
    processes: Vec<ObjId>,
    output: Output,
    silence: SilenceWatch,
}

fn build_control(
    processes: &[ObjId],
    muted_tap: bool,
    gain: f32,
    output: &Output,
) -> Result<Control, Error> {
    let tap = Tap::create(processes, muted_tap)?;
    if !tap.is_float_format().map_err(native)? {
        return Err(Error::new(
            ErrorKind::Native,
            "the tap format is not 32-bit float",
        ));
    }
    let aggregate = Aggregate::create(&output.uid, &tap.uid)?;
    let gain = Arc::new(Gain::new(gain));
    let running = Running::start(aggregate.id, Arc::clone(&gain))?;
    Ok(Control {
        _running: running,
        _aggregate: aggregate,
        _tap: tap,
        gain,
        processes: processes.to_vec(),
        output: output.clone(),
        silence: SilenceWatch::default(),
    })
}

pub struct Taps {
    permission: Permission,
    controls: HashMap<String, Control>,
    settings: HashMap<String, (f32, bool)>,
    /// Why per-app control was last switched off for an app (G1), until the
    /// next successful change.
    problem: Option<String>,
}

impl Taps {
    pub fn new() -> Taps {
        Taps {
            permission: Permission::Unknown,
            controls: HashMap::new(),
            settings: HashMap::new(),
            problem: None,
        }
    }

    pub fn permission(&self) -> Permission {
        self.permission
    }

    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The (volume, muted) Lunchpad applied to an app, if any.
    pub fn setting(&self, app: &str) -> Option<(f32, bool)> {
        self.settings.get(app).copied()
    }

    /// Runs on every refresh of the worker: follows process-set changes,
    /// output changes (G2) and silent muted taps (G1). Whenever a tap cannot
    /// be kept or rebuilt, its setting is forgotten too, so the reported state
    /// matches what is heard (unchanged audio). `apps` holds sorted process lists.
    pub fn maintain(&mut self, apps: &HashMap<String, Vec<ObjId>>) {
        if self.controls.is_empty() {
            self.settings.retain(|app, _| apps.contains_key(app));
            return;
        }
        let output = default_output().ok();
        self.retain_apps(apps, output.as_ref());
        self.follow_output(output.as_ref());
        self.watch_silence(Instant::now());
    }

    /// Forgets apps that are gone. When a tapped app's process set changes (a
    /// new helper would otherwise bypass the tap), its tap is rebuilt over the
    /// new set with the same setting. The old tap goes first so two muted taps
    /// never overlap; the app plays unchanged for that moment.
    fn retain_apps(&mut self, apps: &HashMap<String, Vec<ObjId>>, output: Option<&Output>) {
        self.settings.retain(|app, _| apps.contains_key(app));
        self.controls.retain(|app, _| apps.contains_key(app));
        let changed: Vec<String> = self
            .controls
            .iter()
            .filter(|(app, control)| apps.get(*app) != Some(&control.processes))
            .map(|(app, _)| app.clone())
            .collect();
        for app in changed {
            self.controls.remove(&app);
            if let (Some(processes), Some(output)) = (apps.get(&app), output) {
                self.rebuild(&app, processes, output);
            } else {
                self.settings.remove(&app);
            }
        }
    }

    /// G2: a tap's aggregate plays to the output it was built on. When the
    /// default output changes (switch, unplug, re-plug), rebuild on the new
    /// one; with no output at all, drop the tap.
    fn follow_output(&mut self, output: Option<&Output>) {
        let moved: Vec<(String, Route)> = self
            .controls
            .iter()
            .filter_map(|(app, control)| match route(&control.output, output) {
                Route::Keep => None,
                other => Some((app.clone(), other)),
            })
            .collect();
        for (app, route) in moved {
            let Some(control) = self.controls.remove(&app) else {
                continue;
            };
            let processes = control.processes.clone();
            drop(control);
            match route {
                Route::Rebuild(output) => self.rebuild(&app, &processes, &output),
                Route::Keep | Route::Drop => {
                    self.settings.remove(&app);
                }
            }
        }
    }

    /// G1: see [`SilenceWatch`].
    fn watch_silence(&mut self, now: Instant) {
        let silent: Vec<String> = self
            .controls
            .iter_mut()
            .filter_map(|(app, control)| {
                let producing = MacBackend::is_running_output(&control.processes);
                let heard = control.gain.take_peak() > 0.0;
                control
                    .silence
                    .observe(now, producing, heard)
                    .then(|| app.clone())
            })
            .collect();
        for app in silent {
            self.controls.remove(&app);
            self.settings.remove(&app);
            self.permission = Permission::Denied;
            self.problem = Some(format!(
                "Per-app volume for {app} was switched off: its tap stayed silent for {} s while the app was playing, so Lunchpad probably lacks the System Audio Recording permission",
                SILENCE_LIMIT.as_secs()
            ));
        }
    }

    /// Rebuilds a tap with the app's setting; on failure the setting goes too.
    fn rebuild(&mut self, app: &str, processes: &[ObjId], output: &Output) {
        let rebuilt = self.settings.get(app).and_then(|&(volume, muted)| {
            build_control(processes, true, if muted { 0.0 } else { volume }, output).ok()
        });
        match rebuilt {
            Some(control) => {
                self.controls.insert(app.to_string(), control);
            }
            None => {
                self.settings.remove(app);
            }
        }
    }

    pub fn set(
        &mut self,
        app: &str,
        processes: &[ObjId],
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(), Error> {
        if processes.is_empty() {
            return Err(Error::new(
                ErrorKind::Native,
                "the app has no Core Audio process objects",
            ));
        }
        let (old_volume, old_muted) = self.setting(app).unwrap_or((1.0, false));
        let next = (volume.unwrap_or(old_volume), muted.unwrap_or(old_muted));
        if next == (1.0, false) {
            // Unchanged audio needs no tap; dropping it restores the direct path.
            self.controls.remove(app);
            self.settings.remove(app);
            return Ok(());
        }
        let output = default_output()?;
        if self.permission != Permission::Granted {
            if let Err(e) = self.probe(processes, &output) {
                // Silence is ambiguous; earlier evidence of denial (G1) stands.
                if self.permission != Permission::Denied {
                    self.permission = Permission::Unconfirmed;
                }
                return Err(e);
            }
            self.permission = Permission::Granted;
        }
        let gain = if next.1 { 0.0 } else { next.0 };
        match self.controls.get(app) {
            Some(control) if control.processes == processes && control.output == output => {
                control.gain.set(gain)
            }
            _ => {
                self.controls.remove(app);
                let control = build_control(processes, true, gain, &output)?;
                self.controls.insert(app.to_string(), control);
            }
        }
        self.settings.insert(app.to_string(), next);
        self.problem = None;
        Ok(())
    }

    /// Looks for positive evidence of the permission with an unmuted,
    /// silent-output tap. Never mutes the app.
    fn probe(&self, processes: &[ObjId], output: &Output) -> Result<(), Error> {
        if !MacBackend::is_running_output(processes) {
            return Err(Error::new(ErrorKind::Permission, "cannot confirm the System Audio Recording permission while the app is silent; play audio in it and retry"));
        }
        let probe = build_control(processes, false, 0.0, output)?;
        let started = Instant::now();
        while started.elapsed() < PROBE_WINDOW {
            if probe.gain.peak() > 0.0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(Error::new(
            ErrorKind::Permission,
            "the tap delivered only silence: Lunchpad probably lacks the System Audio Recording permission (silence cannot be told apart from denial)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(object: ObjId, uid: &str) -> Output {
        Output {
            object,
            uid: uid.into(),
        }
    }

    #[test]
    fn taps_follow_the_default_output_and_drop_without_one() {
        let speakers = output(40, "BuiltInSpeakerDevice");
        assert_eq!(route(&speakers, Some(&speakers)), Route::Keep);
        let headphones = output(77, "USB-Headset");
        assert_eq!(
            route(&speakers, Some(&headphones)),
            Route::Rebuild(headphones)
        );
        // Unplugged and plugged back: same UID, new object, so the old aggregate is dead.
        let replugged = output(81, "BuiltInSpeakerDevice");
        assert_eq!(
            route(&speakers, Some(&replugged)),
            Route::Rebuild(replugged)
        );
        assert_eq!(route(&speakers, None), Route::Drop);
    }

    #[test]
    fn a_muted_tap_fails_open_after_a_second_of_silence_while_the_app_plays() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut watch = SilenceWatch::default();
        assert!(!watch.observe(at(0), true, false));
        assert!(!watch.observe(at(750), true, false));
        assert!(
            watch.observe(at(1000), true, false),
            "one second of silence while playing"
        );

        // Sound resets the clock; so does the app going quiet (silence is then expected).
        let mut watch = SilenceWatch::default();
        assert!(!watch.observe(at(0), true, false));
        assert!(!watch.observe(at(900), true, true));
        assert!(!watch.observe(at(1500), true, false));
        assert!(!watch.observe(at(2400), false, false));
        assert!(!watch.observe(at(3000), true, false));
        assert!(!watch.observe(at(3900), true, false));
        assert!(watch.observe(at(4000), true, false));
    }

    #[test]
    fn taps_start_without_permission_and_never_touch_the_os_when_unused() {
        let mut taps = Taps::new();
        assert_eq!(taps.permission(), Permission::Unknown);
        assert_eq!(taps.problem(), None);
        // No controls: maintenance only forgets settings, it does not query Core Audio.
        taps.settings.insert("gone".into(), (0.5, false));
        taps.maintain(&HashMap::new());
        assert_eq!(taps.setting("gone"), None);
        // Unchanged audio and empty process lists never create a tap.
        assert_eq!(
            taps.set("app", &[], Some(0.5), None).map_err(|e| e.kind),
            Err(ErrorKind::Native)
        );
        assert_eq!(taps.set("app", &[1234], Some(1.0), Some(false)), Ok(()));
        assert!(taps.controls.is_empty());
    }
}
