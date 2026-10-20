use super::FineTune;
use crate::live::LiveHandle;
use crate::macros::MacroEngine;
use crate::system_audio::SystemAudio;
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

pub struct FineTuneBridge {
    stop: CancellationToken,
}
impl Drop for FineTuneBridge {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl FineTuneBridge {
    pub fn spawn(
        fine: &FineTune,
        live: LiveHandle,
        engine: MacroEngine,
        audio: SystemAudio,
        app: Option<AppHandle>,
    ) -> Self {
        let mut changes = fine.subscribe();
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let state = changes.borrow_and_update().clone();
                live.update_fine_tune(state.clone());
                engine.follow_system_audio(&audio.snapshot());
                if let Some(app) = &app {
                    let _ = app.emit("fine-tune:state", &state);
                }
                tokio::select! { biased; _ = stopping.cancelled() => break, result = changes.changed() => if result.is_err() { break; } }
            }
            let state = changes.borrow().clone();
            live.update_fine_tune(state.clone());
            engine.follow_system_audio(&audio.snapshot());
            if let Some(app) = &app {
                let _ = app.emit("fine-tune:state", &state);
            }
        });
        Self { stop }
    }
}
