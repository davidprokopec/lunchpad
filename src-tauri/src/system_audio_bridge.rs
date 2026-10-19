//! Keeps Lunchpad in step with the system audio: on every change the backend
//! reports (bursts coalesced, the newest state wins) it stores the snapshot in
//! the live status (state-linked pads and bound faders repaint), moves every
//! bound fader to its target's level without running anything, and tells the
//! window.

use crate::live::LiveHandle;
use crate::macros::MacroEngine;
use crate::profile::SharedProfile;
use crate::system_audio::{read_state, report, AudioReport, SystemAudio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub const EVENT_SYSTEM_AUDIO: &str = "system-audio:changed";
/// Changes closer together than this become one refresh.
const COALESCE: Duration = Duration::from_millis(30);

/// Everything the bridge updates.
#[derive(Clone)]
pub struct Bridge {
    pub audio: SystemAudio,
    pub live: LiveHandle,
    pub engine: MacroEngine,
    pub profile: SharedProfile,
    pub app: Option<AppHandle>,
}

pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Bridge {
    /// Follow the backend on a thread of its own until its subscription ends.
    pub fn spawn(self) -> BridgeHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let changes = self.audio.port().subscribe();
        let spawned = std::thread::Builder::new()
            .name("lunchpad-system-audio".into())
            .spawn(move || {
                self.refresh();
                while !stopping.load(Ordering::Acquire) {
                    match changes.recv_timeout(COALESCE) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    let until = Instant::now() + COALESCE;
                    while changes
                        .recv_timeout(until.saturating_duration_since(Instant::now()))
                        .is_ok()
                    {
                        if Instant::now() >= until {
                            break;
                        }
                    }
                    self.refresh();
                }
                // Native shutdown closes its hub; publish the terminal unavailable state.
                self.refresh();
                tracing::debug!("system audio subscription ended");
            });
        let worker = match spawned {
            Ok(worker) => Some(worker),
            Err(e) => {
                tracing::warn!(error = %e, "the system audio bridge could not start");
                None
            }
        };
        BridgeHandle { stop, worker }
    }

    /// Take one fresh look at the system and pass it on.
    pub fn refresh(&self) -> AudioReport {
        let port = self.audio.port();
        let status = port.status();
        let state = read_state(port.as_ref());
        self.live.update_system(|s| *s = state.clone());
        self.engine.follow_system_audio(&state);
        let report = {
            let store = self.profile.lock();
            report(status, &state, &store.profile)
        };
        if let Some(app) = &self.app {
            let _ = app.emit(EVENT_SYSTEM_AUDIO, &report);
        }
        report
    }
}
