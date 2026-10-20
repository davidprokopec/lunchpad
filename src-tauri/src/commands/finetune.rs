use super::{err, AppState, CmdResult};
use crate::config::{FineTuneSettings, Settings, SharedSettings};
use crate::finetune::{FineTune, FineTuneState};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn fine_tune_state(state: State<'_, AppState>) -> FineTuneState {
    state.fine_tune.state()
}
#[tauri::command]
pub fn fine_tune_connect(state: State<'_, AppState>) -> FineTuneState {
    state.fine_tune.connect()
}
#[tauri::command]
pub fn fine_tune_disconnect(state: State<'_, AppState>) -> FineTuneState {
    state.fine_tune.disconnect()
}
#[tauri::command]
pub fn fine_tune_refresh(state: State<'_, AppState>) -> FineTuneState {
    state.fine_tune.refresh()
}

pub(super) fn update(
    settings: &SharedSettings,
    fine: &FineTune,
    config: FineTuneSettings,
) -> CmdResult<Settings> {
    let mut store = settings.lock();
    let before = store.settings.fine_tune.clone();
    store.settings.fine_tune = config.clone();
    if let Err(error) = store.save() {
        store.settings.fine_tune = before;
        return Err(err(error));
    }
    let result = store.settings.clone();
    fine.configure(config);
    Ok(result)
}

#[tauri::command]
pub fn set_fine_tune_settings(
    config: FineTuneSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<Settings> {
    let settings = update(&state.settings, &state.fine_tune, config)?;
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
    fn settings_persist_first_preserve_other_integrations_and_rollback_only_fine_tune() {
        let dir =
            std::env::temp_dir().join(format!("lunchpad-fine-settings-{}", uuid::Uuid::new_v4()));
        let settings = Arc::new(Mutex::new(SettingsStore::load(&dir)));
        settings.lock().settings.audio.output_device = Some("preserved".into());
        settings.lock().settings.wave_link.port = Some(1888);
        let fine = FineTune::start(FineTuneSettings::default());
        let config = FineTuneSettings {
            enabled: true,
            auto_connect: false,
        };
        update(&settings, &fine, config.clone()).unwrap();
        let saved = SettingsStore::load(&dir).settings;
        assert_eq!(saved.fine_tune, config);
        assert_eq!(saved.audio.output_device.as_deref(), Some("preserved"));
        assert!(!saved.audio.macos_app_volume);
        assert_eq!(saved.wave_link.port, Some(1888));
        let expected_status = if cfg!(target_os = "macos") {
            crate::finetune::FineTuneStatus::Disconnected
        } else {
            crate::finetune::FineTuneStatus::Unsupported
        };
        assert_eq!(fine.state().status, expected_status);
        std::fs::remove_file(dir.join("settings.json")).unwrap();
        std::fs::create_dir(dir.join("settings.json")).unwrap();
        assert!(update(&settings, &fine, FineTuneSettings::default()).is_err());
        assert_eq!(settings.lock().settings.fine_tune, config);
        assert_eq!(fine.state().status, expected_status);
        fine.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
