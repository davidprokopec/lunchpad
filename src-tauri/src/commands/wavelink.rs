use super::{err, AppState, CmdResult};
use crate::config::{Settings, SharedSettings, WaveLinkSettings};
use crate::wavelink::{WaveLink, WaveLinkState};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn wave_link_state(state: State<'_, AppState>) -> WaveLinkState {
    state.wave_link.state()
}
#[tauri::command]
pub fn wave_link_connect(state: State<'_, AppState>) -> WaveLinkState {
    state.wave_link.connect()
}
#[tauri::command]
pub fn wave_link_disconnect(state: State<'_, AppState>) -> WaveLinkState {
    state.wave_link.disconnect()
}
#[tauri::command]
pub fn wave_link_refresh(state: State<'_, AppState>) -> WaveLinkState {
    state.wave_link.refresh()
}

pub(super) fn update(
    settings: &SharedSettings,
    wave: &WaveLink,
    config: WaveLinkSettings,
) -> CmdResult<Settings> {
    config.validate()?;
    let mut store = settings.lock();
    let before = store.settings.wave_link.clone();
    store.settings.wave_link = config.clone();
    if let Err(error) = store.save() {
        store.settings.wave_link = before;
        return Err(err(error));
    }
    let result = store.settings.clone();
    // Serialize configuration with persistence, but never wait on network I/O.
    wave.configure(config)?;
    Ok(result)
}

#[tauri::command]
pub fn set_wave_link_settings(
    config: WaveLinkSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<Settings> {
    let settings = update(&state.settings, &state.wave_link, config)?;
    let _ = app.emit(super::settings::EVENT_SETTINGS, &settings);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SettingsStore;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn settings_persist_only_wave_link_and_save_failure_rolls_back_without_connecting() {
        let dir =
            std::env::temp_dir().join(format!("lunchpad-wave-settings-{}", uuid::Uuid::new_v4()));
        let settings = Arc::new(Mutex::new(SettingsStore::load(&dir)));
        settings.lock().settings.audio.output_device = Some("preserved".into());
        let wave = WaveLink::start(WaveLinkSettings::default());
        let config = WaveLinkSettings {
            enabled: true,
            auto_connect: false,
            port: Some(12345),
        };
        update(&settings, &wave, config.clone()).unwrap();
        let saved = SettingsStore::load(&dir).settings;
        assert_eq!(saved.wave_link, config);
        assert_eq!(saved.audio.output_device.as_deref(), Some("preserved"));
        assert!(!saved.audio.macos_app_volume);
        assert_eq!(
            wave.state().status,
            crate::wavelink::WaveLinkStatus::Disconnected
        );
        let invalid = WaveLinkSettings {
            port: Some(0),
            ..config.clone()
        };
        assert!(update(&settings, &wave, invalid).is_err());
        assert_eq!(settings.lock().settings.wave_link, config);
        std::fs::remove_file(dir.join("settings.json")).unwrap();
        std::fs::create_dir(dir.join("settings.json")).unwrap();
        assert!(update(&settings, &wave, WaveLinkSettings::default()).is_err());
        assert_eq!(settings.lock().settings.wave_link, config);
        assert_eq!(
            wave.state().status,
            crate::wavelink::WaveLinkStatus::Disconnected
        );
        wave.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
