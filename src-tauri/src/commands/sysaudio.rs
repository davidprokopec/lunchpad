//! System audio commands (read-only for now). Live changes arrive as
//! `audio:state` events (see `crate::sysaudio::EVENT_AUDIO_STATE`).

use super::{err, AppState, CmdResult};
use crate::sysaudio::{App, Device};
use tauri::State;

/// Input and output devices with their volume, mute and default flags.
#[tauri::command]
pub async fn sysaudio_devices(state: State<'_, AppState>) -> CmdResult<Vec<Device>> {
    state.sysaudio.devices().map_err(err)
}

/// Apps that play audio, for picking one explicitly.
#[tauri::command]
pub async fn sysaudio_apps(state: State<'_, AppState>) -> CmdResult<Vec<App>> {
    state.sysaudio.apps().map_err(err)
}
