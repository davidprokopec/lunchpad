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
//! granted or denied. After that, a conditional watchdog (`SilenceWatch`)
//! removes silent taps when the process reports running output. That property
//! is unverified while tap-muted; a false report may prevent detection. This
//! path remains experimental, opt-in and default-off pending user-present
//! revocation/capture-failure/output-switch testing, not guaranteed fail-open.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use super::engine::{Error, ErrorKind, Permission};
use super::macos::{
    check, get, get_string, CaResult, MacBackend, OSStatus, ObjId, DEV_UID, HW_DEFAULT_OUTPUT,
    HW_PROCESS_LIST, SYSTEM_OBJECT, TAP_FORMAT,
};
use super::model::Member;

type CFRef = *const c_void;

const PROBE_WINDOW: Duration = Duration::from_millis(400);
const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_FLAG_IS_FLOAT: u32 = 1;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Asbd {
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

impl Asbd {
    pub(super) fn log(&self, object: ObjId, direction: &str, index: usize) {
        tracing::info!(object, direction, index, channels = self.channels_per_frame,
            float = self.format_id == FORMAT_LINEAR_PCM && self.format_flags & FORMAT_FLAG_IS_FLOAT != 0,
            interleaved = self.format_flags & (1 << 5) == 0, format = ?self,
            "app audio stream format");
    }
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
        format.log(self.id, "tap", 0);
        Ok(format.format_id == FORMAT_LINEAR_PCM
            && format.format_flags & FORMAT_FLAG_IS_FLOAT != 0
            && format.bits_per_channel == 32)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        tracing::info!(tap = self.id, "app audio tap destroying");
        // SAFETY: `id` is a tap this struct created and has not destroyed.
        let status = unsafe { AudioHardwareDestroyProcessTap(self.id) };
        if let Err(error) = check("AudioHardwareDestroyProcessTap", status) {
            tracing::warn!(tap = self.id, ?error, "app audio tap teardown failed");
        }
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
        tracing::info!(aggregate = self.id, "app audio aggregate destroying");
        // SAFETY: `id` is an aggregate this struct created and has not destroyed.
        let status = unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
        if let Err(error) = check("AudioHardwareDestroyAggregateDevice", status) {
            tracing::warn!(
                aggregate = self.id,
                ?error,
                "app audio aggregate teardown failed"
            );
        }
    }
}

/// Shared with the real-time IOProc: atomics only, no locks, no allocation.
struct Gain {
    gain_bits: AtomicU32,
    peak_bits: AtomicU32,
    callbacks: AtomicU64,
    summary_input_bits: AtomicU32,
    summary_output_bits: AtomicU32,
    input_buffers: AtomicU32,
    first_input_channels: AtomicU32,
    output_buffers: AtomicU32,
}

impl Gain {
    fn new(gain: f32) -> Gain {
        Gain {
            gain_bits: AtomicU32::new(gain.to_bits()),
            peak_bits: AtomicU32::new(0),
            callbacks: AtomicU64::new(0),
            summary_input_bits: AtomicU32::new(0),
            summary_output_bits: AtomicU32::new(0),
            input_buffers: AtomicU32::new(0),
            first_input_channels: AtomicU32::new(0),
            output_buffers: AtomicU32::new(0),
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

    /// Diagnostic windows are independent of the watchdog's input peak.
    fn take_summary(&self) -> (u64, f32, f32) {
        (
            self.callbacks.swap(0, Ordering::Relaxed),
            f32::from_bits(self.summary_input_bits.swap(0, Ordering::Relaxed)),
            f32::from_bits(self.summary_output_bits.swap(0, Ordering::Relaxed)),
        )
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
    gain_state
        .input_buffers
        .store(ins.len() as u32, Ordering::Relaxed);
    gain_state
        .first_input_channels
        .store(ins.first().map_or(0, |b| b.channels), Ordering::Relaxed);
    gain_state
        .output_buffers
        .store(outs.len() as u32, Ordering::Relaxed);
    // SAFETY: the tap format was checked to be 32-bit float, so the input
    // buffer holds `byte_size / 4` f32 samples.
    let samples: &[f32] = input.map_or(&[], |b| unsafe {
        std::slice::from_raw_parts(b.data as *const f32, b.byte_size as usize / 4)
    });
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    let mut output_peak = 0f32;
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
                output_peak = output_peak.max(o[f * out_channels + c].abs());
            }
        }
        o[frames * out_channels..].fill(0.0);
    }
    if peak > gain_state.peak() {
        gain_state
            .peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
    }
    gain_state.callbacks.fetch_add(1, Ordering::Relaxed);
    gain_state
        .summary_input_bits
        .fetch_max(peak.to_bits(), Ordering::Relaxed);
    gain_state
        .summary_output_bits
        .fetch_max(output_peak.to_bits(), Ordering::Relaxed);
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
        tracing::info!(
            aggregate = self.device,
            "app audio IOProc stopping/destroying"
        );
        // SAFETY: stop then destroy the IOProc this struct created; after
        // destroy the HAL no longer calls it, so the Arc may be released.
        unsafe {
            let status = AudioDeviceStop(self.device, self.id);
            if let Err(error) = check("AudioDeviceStop", status) {
                tracing::warn!(
                    aggregate = self.device,
                    ?error,
                    "app audio IOProc stop failed"
                );
            }
            let status = AudioDeviceDestroyIOProcID(self.device, self.id);
            if let Err(error) = check("AudioDeviceDestroyIOProcID", status) {
                tracing::warn!(
                    aggregate = self.device,
                    ?error,
                    "app audio IOProc teardown failed"
                );
            }
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

/// Experimental G1 watchdog. A muted tap without the System Audio Recording
/// permission delivers zeros while the app stays muted, so the app would go
/// silent while Lunchpad reports success. If the tap's input stays silent for
/// `SILENCE_LIMIT` while the app is producing output, the tap is dropped.
/// This is conditional: IsRunningOutput may itself be false for a tap-muted
/// app. It does not prove playback or guarantee revocation recovery. Taps stay
/// opt-in/default-off pending user-present capture/revocation/route testing.
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

/// The OS resource lifetime is injectable; production field drop order stays
/// IOProc, aggregate, tap. Tests substitute a drop-counted resource, no taps.
trait ControlResources {}

struct NativeResources {
    _running: Running,
    _aggregate: Aggregate,
    _tap: Tap,
}

impl ControlResources for NativeResources {}

struct Control {
    _resources: Box<dyn ControlResources>,
    gain: Arc<Gain>,
    processes: Vec<ObjId>,
    output: Output,
    silence: SilenceWatch,
    last_summary: Instant,
}

fn build_control(
    processes: &[ObjId],
    muted_tap: bool,
    gain: f32,
    output: &Output,
) -> Result<Control, Error> {
    let tap = Tap::create(processes, muted_tap)?;
    tracing::info!(tap = tap.id, ?processes, muted_tap, gain, output_uid = %output.uid, "app audio tap created");
    if !tap.is_float_format().map_err(native)? {
        return Err(Error::new(
            ErrorKind::Native,
            "the tap format is not 32-bit float",
        ));
    }
    let aggregate = Aggregate::create(&output.uid, &tap.uid)?;
    tracing::info!(tap = tap.id, aggregate = aggregate.id, output_uid = %output.uid,
        input_buffer_index = 0, input_selection = "ins.first()", "app audio aggregate created");
    super::macos::log_aggregate_streams(aggregate.id);
    let gain = Arc::new(Gain::new(gain));
    let running = Running::start(aggregate.id, Arc::clone(&gain))?;
    Ok(Control {
        _resources: Box::new(NativeResources {
            _running: running,
            _aggregate: aggregate,
            _tap: tap,
        }),
        gain,
        processes: processes.to_vec(),
        output: output.clone(),
        silence: SilenceWatch::default(),
        last_summary: Instant::now(),
    })
}

/// Narrow seam for failure-injected maintenance tests. Calls still run only
/// on the audio worker, never under engine locks.
trait TapSystem {
    fn processes(&self) -> Result<Vec<ObjId>, Error>;
    fn output(&self) -> Result<Output, Error>;
    fn running(&self, processes: &[ObjId]) -> Result<bool, Error>;
    fn build(&self, processes: &[ObjId], gain: f32, output: &Output) -> Result<Control, Error>;
    /// Unmuted, silent-output positive-signal check. Never mutes the app.
    fn probe(&self, processes: &[ObjId], output: &Output) -> Result<(), Error>;
}

struct CoreAudio;

impl TapSystem for CoreAudio {
    fn processes(&self) -> Result<Vec<ObjId>, Error> {
        super::macos::get_ids(SYSTEM_OBJECT, HW_PROCESS_LIST).map_err(native)
    }
    fn output(&self) -> Result<Output, Error> {
        default_output()
    }
    fn running(&self, processes: &[ObjId]) -> Result<bool, Error> {
        MacBackend::is_running_output(processes)
    }
    fn build(&self, processes: &[ObjId], gain: f32, output: &Output) -> Result<Control, Error> {
        build_control(processes, true, gain, output)
    }
    fn probe(&self, processes: &[ObjId], output: &Output) -> Result<(), Error> {
        let probe_started = Instant::now();
        tracing::info!(?processes, output_uid = %output.uid, "app audio probe start");
        let running = self.running(processes).inspect_err(|error| {
            tracing::warn!(?processes, %error, elapsed_ms = probe_started.elapsed().as_millis(), callbacks = 0, peak = 0.0, "app audio probe end: activity error");
        })?;
        tracing::info!(
            ?processes,
            running_output = running,
            "app audio probe activity"
        );
        if !running {
            tracing::warn!(
                ?processes,
                running_output = running,
                elapsed_ms = probe_started.elapsed().as_millis(),
                callbacks = 0,
                peak = 0.0,
                "app audio probe end: silent app"
            );
            return Err(Error::new(ErrorKind::Permission, "cannot confirm the System Audio Recording permission while the app is silent; play audio in it and retry"));
        }
        let probe = build_control(processes, false, 0.0, output).inspect_err(|error| {
            tracing::warn!(?processes, %error, elapsed_ms = probe_started.elapsed().as_millis(), "app audio probe end: construction failed");
        })?;
        let started = Instant::now();
        while started.elapsed() < PROBE_WINDOW {
            if probe.gain.peak() > 0.0 {
                tracing::info!(
                    ?processes,
                    running_output = running,
                    elapsed_ms = probe_started.elapsed().as_millis(),
                    callbacks = probe.gain.callbacks.load(Ordering::Relaxed),
                    peak = probe.gain.peak(),
                    input_buffers = probe.gain.input_buffers.load(Ordering::Relaxed),
                    first_input_channels = probe.gain.first_input_channels.load(Ordering::Relaxed),
                    output_buffers = probe.gain.output_buffers.load(Ordering::Relaxed),
                    "app audio probe end: signal confirmed"
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        tracing::warn!(
            ?processes,
            running_output = running,
            elapsed_ms = probe_started.elapsed().as_millis(),
            callbacks = probe.gain.callbacks.load(Ordering::Relaxed),
            peak = probe.gain.peak(),
            input_buffers = probe.gain.input_buffers.load(Ordering::Relaxed),
            first_input_channels = probe.gain.first_input_channels.load(Ordering::Relaxed),
            output_buffers = probe.gain.output_buffers.load(Ordering::Relaxed),
            "app audio probe end: only silence"
        );
        Err(Error::new(ErrorKind::Permission, "the tap delivered only silence: Lunchpad probably lacks the System Audio Recording permission (silence cannot be told apart from denial)"))
    }
}

pub struct Taps {
    system: Box<dyn TapSystem>,
    permission: Permission,
    controls: HashMap<String, Control>,
    settings: HashMap<String, (f32, bool)>,
    /// Why per-app control was last switched off for an app (G1), until the
    /// next successful change.
    problem: Option<String>,
}

impl Drop for Taps {
    fn drop(&mut self) {
        for app in self.controls.keys() {
            tracing::info!(
                app,
                reason = "tap manager dropped (configure off/shutdown)",
                "app audio control removed"
            );
        }
    }
}

impl Taps {
    pub fn new() -> Taps {
        Self::with_system(Box::new(CoreAudio))
    }

    fn with_system(system: Box<dyn TapSystem>) -> Taps {
        Taps {
            system,
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

    /// Without a trustworthy process list we cannot maintain existing taps.
    /// Restore the direct path before exposing the enumeration failure.
    pub fn process_list(&mut self) -> Result<Vec<ObjId>, Error> {
        self.verified(self.system.processes())
    }

    pub fn active(&mut self, process: ObjId) -> Result<bool, Error> {
        self.verified(self.system.running(&[process]))
    }

    fn verified<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        result.inspect_err(|error| {
            tracing::warn!(%error, "app audio process enumeration/activity verification failed");
            for app in self.controls.keys() {
                tracing::warn!(app, %error, reason = "process verification failed", "app audio control removed");
            }
            self.controls.clear();
            self.settings.clear();
            self.permission = Permission::Unconfirmed;
            self.problem = Some(format!(
                "Process taps were removed because app processes could not be verified: {error}"
            ));
        })
    }

    pub fn apply_settings(&self, members: &mut [Member]) {
        for member in members {
            let (volume, muted) = self.setting(&member.app_id).unwrap_or((1.0, false));
            member.volume = volume;
            member.muted = muted;
        }
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
        let output = self
            .system
            .output()
            .inspect_err(|error| {
                tracing::warn!(%error, "app audio maintenance output unavailable");
            })
            .ok();
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
        self.controls.retain(|app, _| {
            if !apps.contains_key(app) {
                tracing::info!(app, reason = "app disappeared", "app audio control removed");
            }
            apps.contains_key(app)
        });
        let changed: Vec<String> = self
            .controls
            .iter()
            .filter(|(app, control)| apps.get(*app) != Some(&control.processes))
            .map(|(app, _)| app.clone())
            .collect();
        for app in changed {
            tracing::info!(app, old_processes = ?self.controls.get(&app).map(|c| &c.processes),
                new_processes = ?apps.get(&app), reason = "process set changed", "app audio control removed for rebuild");
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
            tracing::info!(app, old_output = ?control.output, ?route, reason = "default output changed/lost", "app audio control removed");
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
        let silent: Vec<(String, Option<Error>)> = self
            .controls
            .iter_mut()
            .filter_map(|(app, control)| {
                let producing = self.system.running(&control.processes);
                if now.duration_since(control.last_summary) >= Duration::from_secs(5) {
                    let (callbacks, input_peak, output_peak) = control.gain.take_summary();
                    tracing::info!(app,
                        gain = f32::from_bits(control.gain.gain_bits.load(Ordering::Relaxed)),
                        callbacks, input_peak, output_peak,
                        input_buffers = control.gain.input_buffers.load(Ordering::Relaxed),
                        first_input_channels = control.gain.first_input_channels.load(Ordering::Relaxed),
                        output_buffers = control.gain.output_buffers.load(Ordering::Relaxed),
                        running_output = ?producing, elapsed_ms = now.duration_since(control.last_summary).as_millis(),
                        "app audio control summary");
                    control.last_summary = now;
                }
                let heard = control.gain.take_peak() > 0.0;
                match producing {
                    Ok(producing) => control
                        .silence
                        .observe(now, producing, heard)
                        .then(|| (app.clone(), None)),
                    Err(error) => Some((app.clone(), Some(error))),
                }
            })
            .collect();
        for (app, reason) in silent {
            self.controls.remove(&app);
            self.settings.remove(&app);
            if let Some(error) = reason {
                tracing::warn!(app, %error, reason = "output activity verification failed", "app audio control removed");
                self.permission = Permission::Unconfirmed;
                self.problem = Some(format!("Process tap for {app} was removed because output activity could not be verified: {error}"));
            } else {
                tracing::warn!(
                    app,
                    reason = "watchdog: silent tap while running output",
                    "app audio control removed"
                );
                self.permission = Permission::Denied;
                self.problem = Some(format!(
                "Per-app volume for {app} was switched off: its tap stayed silent for {} s while the app was playing, so Lunchpad probably lacks the System Audio Recording permission",
                SILENCE_LIMIT.as_secs()
                ));
            }
        }
    }

    /// Rebuilds a tap with the app's setting; on failure the setting goes too.
    fn rebuild(&mut self, app: &str, processes: &[ObjId], output: &Output) {
        if let Some((volume, muted)) = self.settings.remove(app) {
            tracing::info!(app, ?processes, output = ?output, volume, muted, "app audio rebuild start");
            match self
                .system
                .build(processes, if muted { 0.0 } else { volume }, output)
            {
                Ok(control) => {
                    self.controls.insert(app.to_string(), control);
                    self.settings.insert(app.to_string(), (volume, muted));
                    tracing::info!(app, "app audio rebuild succeeded");
                }
                Err(error) => {
                    tracing::warn!(app, %error, reason = "rebuild failed; setting forgotten", "app audio rebuild failed")
                }
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
        tracing::info!(app, ?processes, requested_volume = ?volume, requested_mute = ?muted,
            permission = ?self.permission, "app audio setter");
        if processes.is_empty() {
            tracing::warn!(app, "app audio setter rejected: no process objects");
            return Err(Error::new(
                ErrorKind::Native,
                "the app has no Core Audio process objects",
            ));
        }
        let (old_volume, old_muted) = self.setting(app).unwrap_or((1.0, false));
        let next = (volume.unwrap_or(old_volume), muted.unwrap_or(old_muted));
        if next == (1.0, false) {
            tracing::info!(
                app,
                had_control = self.controls.contains_key(app),
                reason = "unity/unmuted: direct path, no tap",
                "app audio setter unity/no-op or removal"
            );
            // Unchanged audio needs no tap; dropping it restores the direct path.
            self.controls.remove(app);
            self.settings.remove(app);
            return Ok(());
        }
        let output = self.system.output().inspect_err(|error| {
            tracing::warn!(app, %error, reason = "setter output unavailable", "app audio control removed");
            self.controls.remove(app);
            self.settings.remove(app);
        })?;
        if self
            .controls
            .get(app)
            .is_some_and(|control| control.processes != processes || control.output != output)
        {
            tracing::info!(
                app,
                ?processes,
                ?output,
                reason = "setter process set/output mismatch",
                "app audio control removed for rebuild"
            );
            self.controls.remove(app);
            self.settings.remove(app);
        }
        if self.permission != Permission::Granted {
            tracing::info!(app, ?processes, "app audio setter probing permission");
            if let Err(e) = self.system.probe(processes, &output) {
                tracing::warn!(app, error = %e, reason = "permission probe failed", "app audio control removed/setter failed");
                self.controls.remove(app);
                self.settings.remove(app);
                // Silence is ambiguous; earlier evidence of denial (G1) stands.
                if self.permission != Permission::Denied {
                    self.permission = Permission::Unconfirmed;
                }
                return Err(e);
            }
            self.permission = Permission::Granted;
            tracing::info!(app, permission = ?self.permission, "app audio setter probe confirmed");
        }
        let gain = if next.1 { 0.0 } else { next.0 };
        match self.controls.get(app) {
            Some(control) if control.processes == processes && control.output == output => {
                tracing::info!(app, gain, "app audio setter existing control gain update");
                control.gain.set(gain)
            }
            _ => {
                tracing::info!(
                    app,
                    gain,
                    ?output,
                    ?processes,
                    "app audio setter build start"
                );
                self.controls.remove(app);
                self.settings.remove(app);
                let control = self.system.build(processes, gain, &output).inspect_err(|error| {
                    tracing::warn!(app, %error, "app audio setter build failed; control/setting absent");
                })?;
                self.controls.insert(app.to_string(), control);
                tracing::info!(app, "app audio setter build succeeded");
            }
        }
        self.settings.insert(app.to_string(), next);
        self.problem = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[test]
    fn diagnostic_summary_resets_without_consuming_watchdog_peak() {
        let gain = Gain::new(0.5);
        let samples = [0.8f32, -0.4];
        let mut rendered = [0f32; 2];
        let input = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: 2,
                byte_size: 8,
                data: samples.as_ptr().cast_mut().cast(),
            }],
        };
        let mut output = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: 2,
                byte_size: 8,
                data: rendered.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: local f32 buffers and Gain outlive this synchronous callback;
        // the unused timestamp arguments may be null. No HAL/audio is invoked.
        let status = unsafe {
            io_proc(
                0,
                std::ptr::null(),
                &input,
                std::ptr::null(),
                &mut output,
                std::ptr::null(),
                std::ptr::from_ref(&gain).cast_mut().cast(),
            )
        };
        assert_eq!(status, 0);
        assert_eq!(rendered, [0.4, -0.2]);
        assert_eq!(gain.input_buffers.load(Ordering::Relaxed), 1);
        assert_eq!(gain.first_input_channels.load(Ordering::Relaxed), 2);
        assert_eq!(gain.output_buffers.load(Ordering::Relaxed), 1);
        assert_eq!(gain.take_summary(), (1, 0.8, 0.4));
        assert_eq!(gain.take_summary(), (0, 0.0, 0.0));
        assert_eq!(gain.take_peak(), 0.8);
        assert_eq!(f32::from_bits(gain.gain_bits.load(Ordering::Relaxed)), 0.5);
    }

    struct FakeResource {
        live: Rc<Cell<usize>>,
        dropped: Rc<Cell<usize>>,
    }

    impl ControlResources for FakeResource {}
    impl Drop for FakeResource {
        fn drop(&mut self) {
            self.live.set(self.live.get() - 1);
            self.dropped.set(self.dropped.get() + 1);
        }
    }

    struct FakeState {
        output: Result<Output, Error>,
        processes: Result<Vec<ObjId>, Error>,
        running: Result<bool, Error>,
        fail_build: bool,
        live_at_build: Vec<usize>,
        probes: usize,
        probe_result: Result<(), Error>,
    }

    #[derive(Clone)]
    struct FakeSystem {
        state: Rc<RefCell<FakeState>>,
        live: Rc<Cell<usize>>,
        dropped: Rc<Cell<usize>>,
    }

    impl FakeSystem {
        fn new() -> Self {
            Self {
                state: Rc::new(RefCell::new(FakeState {
                    output: Ok(output(40, "speakers")),
                    processes: Ok(vec![7]),
                    running: Ok(true),
                    fail_build: false,
                    live_at_build: Vec::new(),
                    probes: 0,
                    probe_result: Ok(()),
                })),
                live: Rc::new(Cell::new(0)),
                dropped: Rc::new(Cell::new(0)),
            }
        }
    }

    impl TapSystem for FakeSystem {
        fn processes(&self) -> Result<Vec<ObjId>, Error> {
            self.state.borrow().processes.clone()
        }
        fn output(&self) -> Result<Output, Error> {
            self.state.borrow().output.clone()
        }
        fn running(&self, _: &[ObjId]) -> Result<bool, Error> {
            self.state.borrow().running.clone()
        }
        fn probe(&self, _: &[ObjId], _: &Output) -> Result<(), Error> {
            let mut state = self.state.borrow_mut();
            state.probes += 1;
            state.probe_result.clone()
        }
        fn build(&self, processes: &[ObjId], gain: f32, output: &Output) -> Result<Control, Error> {
            let mut state = self.state.borrow_mut();
            state.live_at_build.push(self.live.get());
            if state.fail_build {
                return Err(injected("building tap"));
            }
            self.live.set(self.live.get() + 1);
            Ok(Control {
                _resources: Box::new(FakeResource {
                    live: self.live.clone(),
                    dropped: self.dropped.clone(),
                }),
                gain: Arc::new(Gain::new(gain)),
                processes: processes.to_vec(),
                output: output.clone(),
                silence: SilenceWatch::default(),
                last_summary: Instant::now(),
            })
        }
    }

    fn injected(what: &str) -> Error {
        Error::new(ErrorKind::Native, format!("injected error: {what}"))
    }

    fn assert_direct_path(taps: &Taps, apps: &[&str]) {
        let mut members: Vec<Member> = apps
            .iter()
            .map(|app| Member {
                app_id: (*app).into(),
                name: (*app).into(),
                active: true,
                volume: 0.2,
                muted: true,
            })
            .collect();
        taps.apply_settings(&mut members);
        assert!(members.iter().all(|m| m.volume == 1.0 && !m.muted));
        assert!(taps.controls.is_empty() && taps.settings.is_empty());
    }

    #[test]
    fn process_enumeration_error_drops_controls_and_settings_before_returning() {
        for activity_error in [false, true] {
            let os = FakeSystem::new();
            let mut taps = Taps::with_system(Box::new(os.clone()));
            taps.set("a", &[7], Some(0.2), None).unwrap();
            taps.set("b", &[8], None, Some(true)).unwrap();
            assert_eq!(os.live.get(), 2);
            if activity_error {
                os.state.borrow_mut().running = Err(injected("process activity"));
                assert_eq!(taps.active(7), Err(injected("process activity")));
            } else {
                os.state.borrow_mut().processes = Err(injected("process list"));
                assert_eq!(taps.process_list(), Err(injected("process list")));
            }
            assert_eq!(os.live.get(), 0);
            assert_eq!(os.dropped.get(), 2);
            assert_direct_path(&taps, &["a", "b"]);
            assert_eq!(taps.permission(), Permission::Unconfirmed);
            os.state.borrow_mut().processes = Ok(vec![7, 8]);
            assert_eq!(taps.process_list().unwrap(), [7, 8]);
            taps.maintain(&HashMap::from([
                ("a".into(), vec![7]),
                ("b".into(), vec![8]),
            ]));
            assert_direct_path(&taps, &["a", "b"]);
        }
    }

    #[test]
    fn failed_activity_query_is_unknown_and_removes_the_control_not_idle() {
        let os = FakeSystem::new();
        let mut taps = Taps::with_system(Box::new(os.clone()));
        taps.set("a", &[7], Some(0.2), None).unwrap();
        os.state.borrow_mut().running = Err(injected("running output"));
        taps.maintain(&HashMap::from([("a".into(), vec![7])]));
        assert_eq!(os.live.get(), 0);
        assert_eq!(os.dropped.get(), 1);
        assert_direct_path(&taps, &["a"]);
        assert_eq!(taps.permission(), Permission::Unconfirmed);
        assert!(taps.problem().unwrap().contains("running output"));
    }

    #[test]
    fn command_time_output_switch_build_failure_cannot_leave_phantom_settings() {
        for muted in [false, true] {
            let os = FakeSystem::new();
            let mut taps = Taps::with_system(Box::new(os.clone()));
            taps.set("a", &[7], Some(0.2), Some(muted)).unwrap();
            assert_eq!(taps.setting("a"), Some((0.2, muted)));
            os.state.borrow_mut().output = Ok(output(70, "headphones"));
            os.state.borrow_mut().fail_build = true;
            assert_eq!(
                taps.set("a", &[7], Some(0.4), None),
                Err(injected("building tap"))
            );
            assert_eq!(
                os.state.borrow().live_at_build,
                [0, 0],
                "old resources drop before new construction"
            );
            assert_eq!(os.live.get(), 0);
            assert_eq!(os.dropped.get(), 1);
            taps.maintain(&HashMap::from([("a".into(), vec![7])]));
            assert_direct_path(&taps, &["a"]);
            os.state.borrow_mut().fail_build = false;
            taps.set("a", &[7], Some(0.6), None).unwrap();
            assert_eq!(
                taps.setting("a"),
                Some((0.6, false)),
                "failed mute/gain is not resurrected on retry"
            );
        }
    }

    #[test]
    fn failed_reprobe_forgets_an_existing_unverifiable_control() {
        let os = FakeSystem::new();
        let mut taps = Taps::with_system(Box::new(os.clone()));
        taps.set("a", &[7], Some(0.2), Some(true)).unwrap();
        taps.permission = Permission::Unconfirmed;
        os.state.borrow_mut().probe_result = Err(injected("permission probe"));
        assert_eq!(
            taps.set("a", &[7], Some(0.4), None),
            Err(injected("permission probe"))
        );
        assert_eq!(os.live.get(), 0);
        assert_eq!(os.dropped.get(), 1);
        assert_direct_path(&taps, &["a"]);
    }

    #[test]
    fn maintenance_route_loss_or_failed_rebuild_drops_real_control_ownership() {
        for cause in ["output missing", "output changed", "process set changed"] {
            let os = FakeSystem::new();
            let mut taps = Taps::with_system(Box::new(os.clone()));
            taps.set("a", &[7], Some(0.2), Some(true)).unwrap();
            os.state.borrow_mut().fail_build = true;
            let processes = match cause {
                "output missing" => {
                    os.state.borrow_mut().output = Err(injected("output"));
                    vec![7]
                }
                "output changed" => {
                    os.state.borrow_mut().output = Ok(output(70, "headphones"));
                    vec![7]
                }
                _ => vec![7, 8],
            };
            taps.maintain(&HashMap::from([("a".into(), processes)]));
            assert_eq!(os.live.get(), 0);
            assert_eq!(os.dropped.get(), 1);
            assert_direct_path(&taps, &["a"]);
        }
    }

    #[test]
    fn conditional_watchdog_removes_resources_when_running_output_is_observed() {
        let os = FakeSystem::new();
        let mut taps = Taps::with_system(Box::new(os.clone()));
        taps.set("a", &[7], Some(0.2), None).unwrap();
        let start = Instant::now();
        taps.watch_silence(start);
        assert_eq!(os.live.get(), 1);
        taps.watch_silence(start + SILENCE_LIMIT);
        assert_eq!(os.live.get(), 0);
        assert_eq!(os.dropped.get(), 1);
        assert_direct_path(&taps, &["a"]);
        assert_eq!(taps.permission(), Permission::Denied);
        // This only establishes software cleanup for a known running signal.
        // Real tap-muted IsRunningOutput semantics remain a hardware gate.
    }

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
    fn silence_timer_trips_with_known_output_and_resets_on_known_idle() {
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
