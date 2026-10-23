//! Settings → Phone remote.

use super::{AppState, CmdResult};
use crate::remote::{RemotePairing, RemoteStatus};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn remote_status(state: State<'_, AppState>) -> RemoteStatus {
    state.remote.status()
}

/// Turn the remote on or off and choose its port; starts, restarts or stops the server.
#[tauri::command]
pub async fn set_remote_settings(
    enabled: bool,
    port: u16,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<RemoteStatus> {
    let status = state.remote.configure(enabled, port).await?;
    let settings = state.settings.lock().settings.clone();
    let _ = app.emit(super::settings::EVENT_SETTINGS, &settings);
    Ok(status)
}

/// A new one-time pairing code (replaces any earlier one); only while the remote listens.
#[tauri::command]
pub fn remote_start_pairing(state: State<'_, AppState>) -> CmdResult<RemotePairing> {
    state.remote.start_pairing()
}

#[tauri::command]
pub fn remote_cancel_pairing(state: State<'_, AppState>) -> RemoteStatus {
    state.remote.cancel_pairing()
}

/// Forget a paired phone and disconnect it.
#[tauri::command]
pub fn remote_revoke_device(
    device_id: String,
    state: State<'_, AppState>,
) -> CmdResult<RemoteStatus> {
    state.remote.revoke(&device_id)
}
